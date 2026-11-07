#!/usr/bin/env bash
#
# test-host-install.sh — the gate that proves a host install of WeftDB is what the unit says it is.
#
# PAC-11 (1.0 plan): systemd unit, user and env files, and host-install CI. `install-host.sh`
# places the files; this script is the reason to believe it. It runs against a staged destdir, so
# it needs no root, no new account on the machine, and no service started — and it still catches
# the failures that matter, because every one of them is a claim about bytes on disk or about what
# the real server accepts.
#
# Usage: scripts/packaging/test-host-install.sh [--bin PATH] [--keep]
#   --bin PATH   the weft-server binary to install and to run. Default the first of $WEFT_BIN,
#                target/release/weft-server, target/debug/weft-server that exists. Two gates run
#                the binary itself, so a binary is required, not optional.
#   --keep       leave the scratch directory in place and print where it is.
#   -h, --help   this text.
#
# The gates, in order:
#
#   1.  the manifest `install-host.sh --list` prints is the tree that lands on disk — same paths,
#       same modes. Single source of truth, checked rather than assumed.
#   2.  a non-default `--prefix` rewrites `ExecStart=` to that prefix and to nothing else: a unit
#       that starts `/usr/local/bin/weft-server` on a machine whose binary is at
#       `/opt/weftdb/bin/weft-server` fails at start, and no other gate here would see it.
#   3.  an operator's `<prefix>/etc/weftdb/weft-server.env` survives a reinstall. An upgrade that
#       silently resets an operator's configuration is a bad upgrade.
#   4.  every variable name in the shipped env file is a name the real binary accepts, and none of
#       them is a `Test`-tier channel. Black-box against the binary, not against `settings.rs`.
#   5.  gate 4 can fail: an invented name and a Test-tier name must both be refused by exactly the
#       mechanism gate 4 relies on. This is the gate that has caught every green-but-meaningless
#       check in this branch's history, so it is a gate here and not a comment.
#   6.  systemd's own readers accept the installed files: `systemd-analyze verify` on the unit,
#       `systemd-sysusers` and `systemd-tmpfiles` on the conf files, with their stderr inspected
#       because all three are happy to skip a line they do not understand and exit 0 anyway.
#       Verify resolves the unit by name and builds a real transaction, so the two essential
#       targets an install prefix cannot carry are the only complaint not counted against the
#       unit. Skipped, loudly, where systemd is not installed.
#   7.  the hardening in the unit actually runs the server: the properties are read out of the
#       installed unit — not copied into this script, so the test cannot drift from the file it
#       tests — and applied to the real binary under a transient systemd scope. The server must
#       reach its listening line, and `systemctl stop` must take it down as cleanly as the unit's
#       `KillSignal=SIGTERM` claims. Skipped, loudly, where no systemd manager is reachable.
#
# Exit status: 0 when every gate ran and passed, 1 on the first failure, 2 on a usage error, 3
# when no binary is available to test. Skips are printed and never counted as passes.

set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
REPO_ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)
readonly REPO_ROOT

declare -a PASSES=()
declare -a SKIPS=()
WORK=
KEEP=0
BIN=${WEFT_BIN:-}

pass() {
	PASSES+=("$1")
	echo "ok: $1"
}

fail() {
	echo "$SCRIPT_NAME: FAIL: $1" >&2
	if [[ -n $WORK && $KEEP -eq 0 ]]; then
		echo "  rerun with --keep to inspect the scratch tree" >&2
	fi
	exit 1
}

skip() {
	SKIPS+=("$1")
	echo "SKIPPED: $1"
}

cleanup() {
	local status=$?
	if [[ -n $WORK ]]; then
		if ((KEEP)); then
			echo "$SCRIPT_NAME: scratch kept for inspection at $WORK" >&2
		else
			rm -rf "$WORK"
		fi
	fi
	return "$status"
}
trap cleanup EXIT

usage() {
	sed -n '3,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while (($# > 0)); do
	case $1 in
	--bin)
		[[ $# -ge 2 ]] || {
			usage >&2
			exit 2
		}
		BIN=$2
		shift 2
		;;
	--keep)
		KEEP=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "$SCRIPT_NAME: unknown argument: $1" >&2
		usage >&2
		exit 2
		;;
	esac
done

if [[ -z $BIN ]]; then
	for candidate in "$REPO_ROOT/target/release/weft-server" "$REPO_ROOT/target/debug/weft-server"; do
		[[ -x $candidate ]] && {
			BIN=$candidate
			break
		}
	done
fi
[[ -n $BIN ]] || {
	echo "$SCRIPT_NAME: no weft-server binary to test: build one, or pass --bin PATH / WEFT_BIN=PATH" >&2
	exit 3
}
[[ -x $BIN ]] || fail "$BIN is not an executable file"
readonly BIN
readonly INSTALLER="$REPO_ROOT/scripts/packaging/install-host.sh"
readonly ENV_TEMPLATE="$REPO_ROOT/packaging/env/weft-server.env"
readonly SETTINGS="$REPO_ROOT/weft-server/src/settings.rs"
[[ -x $INSTALLER ]] || fail "$INSTALLER is not executable"
[[ -f $ENV_TEMPLATE ]] || fail "$ENV_TEMPLATE is missing"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/weft-host-install-XXXXXX")
readonly WORK
readonly DEST="$WORK/root"
# An address the socket-address parser itself rejects — a host with no port — refused as
# invalid configuration before anything binds, so the server cannot serve even by accident.
# The refusal is recognisable because `weft-server/src/startup/config.rs::bind_addr` formats
# it as `WEFT_SERVER_ADDR="<value>" is not a socket address`: the gate below greps for that
# name, and `weft-server/tests/operator_cli.rs` pins the same text on the binary. Keep the
# three in step, as with the two refusal fragments further down.
readonly DEAD_ADDR=127.0.0.1
readonly STATE="$WORK/state"
mkdir -p "$STATE"

# Run the real server with a set of NAME=value settings exported, and report what it said.
#
# `env -i` keeps the developer's own WEFT_* out of the picture: a machine with WEFT_KEYS_FILE in
# the shell would otherwise pass or fail a gate for reasons the script cannot see.
#
# $1: file of NAME=value lines, one per line. $2: file to write the server's output to.
# Prints the exit status; leaves the refusal text in $2.
probe_server() {
	local names=$1 out=$2 rc=0
	local -a assignments=()
	while read -r line; do
		assignments+=("$line")
	done <"$names"
	# WEFT_SERVER_ADDR is assigned last so nothing in the file can undo it: the probe must run
	# against an address that binds nothing, so the refusal left standing is the one this gate looks for.
	env -i PATH="$PATH" HOME="$WORK" \
		"${assignments[@]}" WEFT_SERVER_ADDR="$DEAD_ADDR" "$BIN" serve >"$out" 2>&1 || rc=$?
	printf '%s' "$rc"
}

# The two refusal fragments below are matched as literals on purpose. If the wording in
# `weft-server/src/settings.rs` changes, they stop matching, and gate 5 — which requires a refusal
# to be recognised — turns red rather than passing silently. Keep them in step with that file.
readonly REFUSAL_UNKNOWN="not a WeftDB" # keep in step with settings.rs
readonly REFUSAL_TEST="test-harness"    # keep in step with settings.rs

# Gates 1 and 2: install at a prefix that is not the default, so the `ExecStart=` rewrite is the
# thing under test rather than an accident of the default. The prefix lives inside the scratch tree
# because nothing here may touch the machine running the test.
readonly PREFIX="$WORK/opt/weftdb"
WEFT_BIN=$BIN "$INSTALLER" --destdir "$DEST" --prefix "$PREFIX" >"$WORK/install.out" 2>&1 ||
	fail "install-host.sh exited non-zero:
$(cat "$WORK/install.out")"

readonly MANIFEST="$WORK/manifest.tsv"
# The manifest must describe the tree the gates just installed, so it is listed with the same
# --prefix the install used; `--list` without it prints the machine's default prefix instead,
# which is not the tree under test.
WEFT_BIN=$BIN "$INSTALLER" --prefix "$PREFIX" --list >"$MANIFEST"
FILE_COUNT=$(wc -l <"$MANIFEST")
readonly FILE_COUNT
((FILE_COUNT >= 9)) || fail "--list printed $FILE_COUNT entries; a host install is at least 9 files"
while IFS=$'\t' read -r _src dst mode; do
	[[ -e "$DEST$dst" ]] || fail "$dst is missing from the installed tree"
	actual=$(stat -c '%a' "$DEST$dst")
	[[ $actual == "${mode#0}" ]] || fail "$dst is mode $actual, the manifest says ${mode#0}"
done <"$MANIFEST"
pass "the tree on disk is the manifest: $FILE_COUNT files, every mode as listed"

readonly INSTALLED_UNIT="$DEST$PREFIX/lib/systemd/system/weft-server.service"
[[ -f $INSTALLED_UNIT ]] || fail "the unit did not land at $PREFIX/lib/systemd/system"
EXEC_START=$(sed -n 's|^ExecStart=||p' "$INSTALLED_UNIT")
readonly EXEC_START
[[ $EXEC_START == "$PREFIX/bin/weft-server" ]] ||
	fail "ExecStart is '$EXEC_START', expected '$PREFIX/bin/weft-server'"
grep -qF "	$EXEC_START	" "$MANIFEST" ||
	fail "ExecStart does not name a binary the manifest installed: $EXEC_START"
pass "ExecStart names the binary this prefix installed, and no other path"

# Gate 3: an operator's env file is never overwritten.
readonly OPERATOR_ENV="$DEST$PREFIX/etc/weftdb/weft-server.env"
[[ -f $OPERATOR_ENV ]] || fail "the operator's env file was not created at $OPERATOR_ENV"
ENV_MODE=$(stat -c '%a' "$OPERATOR_ENV")
readonly ENV_MODE
[[ $ENV_MODE == 640 ]] || fail "the operator's env file is mode $ENV_MODE, expected 640"
printf 'WEFT_MAX_CONNECTIONS=64\n' >"$OPERATOR_ENV"
WEFT_BIN=$BIN "$INSTALLER" --destdir "$DEST" --prefix "$PREFIX" >"$WORK/reinstall.out" 2>&1 ||
	fail "reinstalling over an existing configuration exited non-zero"
grep -qF "left $OPERATOR_ENV alone" "$WORK/reinstall.out" ||
	fail "the reinstall did not say it left the operator's env file alone"
grep -qxF 'WEFT_MAX_CONNECTIONS=64' "$OPERATOR_ENV" ||
	fail "the reinstall changed the operator's env file"
pass "an operator's configuration survives an upgrade; the template stays in share/doc"

# Gate 4: every name in the shipped env file is a name the real server accepts, and the
# defaults the file literally ships start cleanly. Since PAC-5b an empty value on an integer
# name is a refusal, not a default, and the template uses two shapes: `#WEFT_X=value` ships a
# literal default, `#WEFT_X=` documents a name whose default the server keeps internally and
# describes in prose. The two shapes get two probes.
#
# Probe one exports the literal defaults, with the ones that point at the machine redirected
# into the scratch tree, and expects the only refusal left standing to be the probe's own
# unusable address: a shipped default that does not start cleanly fails here, naming itself.
# Probe two gives every documented name — both shapes — a value no shape can accept. PAC-5b
# then refuses those values, but a name the registry does not know is refused as unknown,
# which is precisely what this probe forbids; that is the registry-coverage check.
readonly NAMES="$WORK/names.txt"          # literal defaults, probe one
readonly ALL_NAMES="$WORK/all-names.txt"  # every documented name, probe two
mkdir -p "$WORK/data/store" "$WORK/data/backups"
# The config stage parses the keys file before it ever reaches the address, so
# the scratch keys file must be a well-formed keys document holding one key —
# the shape auth.rs accepts. Nothing is served or connected anywhere; the probe
# still ends in the refusal over the dead address.
printf '%s\n' '{"version":1,"keys":[{"id":"gate-probe","role":"admin","key_hash":"sha256:0000000000000000000000000000000000000000000000000000000000000000"}]}' >"$WORK/api.keys"
grep -oE '^#[[:space:]]*WEFT_[A-Z0-9_]+' "$ENV_TEMPLATE" | sed -E 's/^#[[:space:]]*//' |
	sort -u >"$ALL_NAMES"
NAME_COUNT=$(wc -l <"$ALL_NAMES")
readonly NAME_COUNT
((NAME_COUNT >= 30)) || fail "the env file documents only $NAME_COUNT variables; expected 30 or more"
grep -E '^#WEFT_[A-Z0-9_]+=.+' "$ENV_TEMPLATE" | sed 's/^#//' | sort -u |
	while IFS='=' read -r gname gvalue; do
		case $gname in
			WEFT_DATA_DIR) gvalue="$WORK/data" ;;
			WEFT_SEGMENT_STORE_ROOT) gvalue="$WORK/data/store" ;;
			WEFT_BACKUP_DIR) gvalue="$WORK/data/backups" ;;
			WEFT_KEYS_FILE) gvalue="$WORK/api.keys" ;;
		esac
		printf '%s=%s\n' "$gname" "$gvalue"
	done >"$NAMES"
PROBE_RC=$(probe_server "$NAMES" "$WORK/probe.out")
readonly PROBE_RC
if [[ $PROBE_RC != 78 ]]; then
	fail "the server exited $PROBE_RC with the shipped defaults set; expected 78, a refusal over the unusable address
$(tail -20 "$WORK/probe.out")"
fi
if grep -q "$REFUSAL_UNKNOWN" "$WORK/probe.out" || grep -q "$REFUSAL_TEST" "$WORK/probe.out"; then
	fail "the server refused a name from the shipped env file:
$(grep -e "$REFUSAL_UNKNOWN" -e "$REFUSAL_TEST" "$WORK/probe.out")"
fi
grep -qi "WEFT_SERVER_ADDR" "$WORK/probe.out" ||
	fail "the refusal did not name WEFT_SERVER_ADDR, so it is not the refusal this gate expects:
$(tail -20 "$WORK/probe.out")"
OTHER_NAMED=$(grep -oE 'WEFT_[A-Z0-9_]+' "$WORK/probe.out" | sort -u | grep -cv '^WEFT_SERVER_ADDR$') || true
readonly OTHER_NAMED
((OTHER_NAMED == 0)) ||
	fail "a shipped default was refused besides the probe's own address ($OTHER_NAMED other name(s)):
$(grep -oE 'WEFT_[A-Z0-9_]+' "$WORK/probe.out" | sort -u)"
while read -r gname; do
	printf '%s=__weft_gate_probe__\n' "$gname"
done <"$ALL_NAMES" >"$WORK/sentinel.txt"
COVER_RC=$(probe_server "$WORK/sentinel.txt" "$WORK/cover.out")
readonly COVER_RC
if [[ $COVER_RC != 78 ]]; then
	fail "the server exited $COVER_RC with every documented name set to an impossible value; expected 78
$(tail -20 "$WORK/cover.out")"
fi
if grep -q "$REFUSAL_UNKNOWN" "$WORK/cover.out" || grep -q "$REFUSAL_TEST" "$WORK/cover.out"; then
	fail "a name in the shipped env file is not the real server's name:
$(grep -e "$REFUSAL_UNKNOWN" -e "$REFUSAL_TEST" "$WORK/cover.out")"
fi
pass "all $NAME_COUNT variable names in the env file are ones the real server accepts, and its literal defaults start cleanly"

# Gate 5: gate 4 is capable of failing. An invented name first.
printf 'WEFT_NOT_A_WEFTDB_VARIABLE=1\n' >"$WORK/invented.txt"
INVENTED_RC=$(probe_server "$WORK/invented.txt" "$WORK/invented.out")
readonly INVENTED_RC
[[ $INVENTED_RC == 78 ]] ||
	fail "an invented variable name was accepted (exit $INVENTED_RC):
$(tail -20 "$WORK/invented.out")"
grep -q "$REFUSAL_UNKNOWN" "$WORK/invented.out" ||
	fail "an invented variable name was refused for some reason other than the one gate 4 looks for, which means gate 4 would not have caught it:
$(tail -20 "$WORK/invented.out")"
pass "an invented variable name is refused, and recognised as refused"

# A Test-tier name next: taken out of the registry rather than invented, because the whole point of
# gate 4 is that the env file cannot carry one, and the only way to know that is to try. The name is
# read off the same line as the tier, never from context lines: every VarSpec is one line, so `-B3`
# also returns the entry sitting above each match, and the probe then points a Stable name — one the
# server is meant to accept — at the Test-tier refusal. On ubuntu-latest that read as "OTEL_EXPORTER
# OTLP_ENDPOINT was refused for some reason other than its tier" while the server was in fact
# correct to accept it.
TEST_NAME=$(grep -oE 'name: "(WEFT|OTEL)[A-Z0-9_]+", tier: Tier::Test' "$SETTINGS" |
	sed -E 's/^name: "([A-Z0-9_]+)".*/\1/' | head -1) || true
readonly TEST_NAME
if [[ -z $TEST_NAME ]]; then
	skip "no Tier::Test entry was found in $SETTINGS, so the env file's Test-tier refusal went untested"
else
	printf '%s=1\n' "$TEST_NAME" >"$WORK/tier.txt"
	TEST_RC=$(probe_server "$WORK/tier.txt" "$WORK/tier.out")
	readonly TEST_RC
	[[ $TEST_RC == 78 ]] ||
		fail "the Test-tier channel $TEST_NAME was accepted on a real server (exit $TEST_RC):
$(tail -20 "$WORK/tier.out")"
	grep -q "$REFUSAL_TEST" "$WORK/tier.out" ||
		fail "$TEST_NAME was refused for some reason other than its tier, which means gate 4 would not have caught it:
$(tail -20 "$WORK/tier.out")"
	pass "the Test-tier channel $TEST_NAME is refused on a real server, which is what keeps it out of the env file"
fi

# Gate 6: systemd's own readers accept the installed files.
if command -v systemd-analyze >/dev/null 2>&1; then
	# The unit is looked up by name, so first assert the installed bytes exist: that keeps "nothing
	# was installed" apart from "systemd dislikes what was installed", which the first version of
	# this gate conflated when it answered "Unit weft-server.service not found" for a unit gate 1
	# had just confirmed on disk.
	UNIT_IN_ROOT="$PREFIX/lib/systemd/system/weft-server.service"
	[[ -f "$DEST$UNIT_IN_ROOT" ]] ||
		fail "the installed tree carries no unit at $DEST$UNIT_IN_ROOT, so systemd has nothing to read:
$(find "$DEST" -name '*.service' -printf '  %p\n' 2>/dev/null || true)"
	# `systemd-analyze verify --root R` does not read a unit from a path: it looks the name up in
	# R's own search paths (R/usr/lib/systemd/system, R/etc/systemd/system), and a prefix install
	# lives in neither. ubuntu-latest said "Unit weft-server.service not found." for
	# $DEST/opt/weftdb/lib/systemd/system/weft-server.service with the file plainly present.
	# So stage the installed bytes where systemd looks, verify the name, and prove the staged copy
	# is the installed file rather than some sibling that happens to parse.
	UNIT_SEARCH_DIR="$DEST/usr/lib/systemd/system"
	mkdir -p "$UNIT_SEARCH_DIR"
	cp "$DEST$UNIT_IN_ROOT" "$UNIT_SEARCH_DIR/weft-server.service"
	cmp -s "$DEST$UNIT_IN_ROOT" "$UNIT_SEARCH_DIR/weft-server.service" ||
		fail "staging the unit for verification altered it: $DEST$UNIT_IN_ROOT and its copy in $UNIT_SEARCH_DIR differ"
	# What remains is that an installed tree carries no essential targets. systemd injects
	# `Requires=sysinit.target` into every service, and `verify --root` builds the real transaction,
	# so on the runner's systemd (255) it answers
	# "weft-server.service: Failed to create weft-server.service/start: Unit sysinit.target not
	# found." An install prefix is not a bootable system and must not carry sysinit.target, so that
	# line is a property of verifying a partial tree, not of the unit. The host running this gate
	# (systemd 262) returns 0 for the same bytes, so the gate cannot simply require exit 0: it would
	# pass here and fail there for reasons nothing to do with WeftDB controls.
	#
	# So tolerate precisely those two lines and fail on anything else. The allowance names the
	# essential targets by name rather than matching "not found" generally, because a unit that
	# `Wants=`s or `Requires=`s some other missing unit is a real defect this gate must keep
	# catching, and `SYSTEMD_RELAX_ESSENTIALS` is not an option: no such string exists in this
	# host's systemd-analyze, libsystemd-shared, or libsystemd-core, and adding an environment
	# variable on the assumption a different version honours it would change the gate's meaning on
	# some runners without proving anything on any of them.
	verify_out=$(systemd-analyze verify --root "$DEST" weft-server.service 2>&1) && verify_rc=0 || verify_rc=$?
	# Delete the two lines rather than grep -v for them: grep exits 1 when it filters out every
	# line, which is the passing case here, and the script runs under `set -euo pipefail`. The
	# first version of this filter therefore aborted the run between gate 5's "ok" and any
	# message at all — CI printed "exit 1" and nothing else, which is a failure that cannot be
	# read. sed's status does not depend on whether it deleted anything.
	verify_blame=$(sed -E '/Unit (sysinit|basic)\.target not found/d; s/^[[:space:]]+//' <<<"$verify_out")
	if [[ $verify_rc -ne 0 && -z $verify_blame ]]; then
		pass "systemd-analyze verify finds no fault in the installed unit beyond the essential targets an install prefix cannot carry"
	elif [[ -n $verify_blame ]]; then
		fail "systemd-analyze verify rejected the installed unit (verified as weft-server.service under --root $DEST, staged from $DEST$UNIT_IN_ROOT into $UNIT_SEARCH_DIR; exit $verify_rc; a missing sysinit.target or basic.target alone is not counted against the unit):
$verify_out"
	else
		pass "systemd-analyze verify is silent about the installed unit"
	fi
else
	skip "systemd-analyze is not installed here, so nothing has ever parsed the unit file"
fi

if command -v systemd-sysusers >/dev/null 2>&1; then
	sysusers_err=$(systemd-sysusers --root "$DEST" --cat-config \
		"$DEST$PREFIX/lib/sysusers.d/weftdb.conf" 2>&1 >/dev/null) && sysusers_rc=0 || sysusers_rc=$?
	[[ $sysusers_rc -eq 0 && -z $sysusers_err ]] ||
		fail "systemd-sysusers rejected weftdb.conf (exit $sysusers_rc):
$sysusers_err"
	pass "systemd-sysusers accepts weftdb.conf, line type and all"
else
	skip "systemd-sysusers is not installed here, so the account line was never parsed"
fi

if command -v systemd-tmpfiles >/dev/null 2>&1; then
	tmpfiles_err=$(systemd-tmpfiles --root "$DEST" --cat-config \
		"$DEST$PREFIX/lib/tmpfiles.d/weftdb.conf" 2>&1 >/dev/null) && tmpfiles_rc=0 || tmpfiles_rc=$?
	[[ $tmpfiles_rc -eq 0 && -z $tmpfiles_err ]] ||
		fail "systemd-tmpfiles rejected weftdb.conf (exit $tmpfiles_rc):
$tmpfiles_err"
	pass "systemd-tmpfiles accepts weftdb.conf"
else
	skip "systemd-tmpfiles is not installed here, so the runtime directory line was never parsed"
fi

# Gate 7: the hardening in the unit runs the server.
#
# The properties come out of the installed unit file rather than being repeated here, because a
# test carrying its own copy of the hardening list passes when the unit loses an entry — which is
# the drift this script exists to prevent.
# `journalctl` is required, not optional, and for a reason that broke this gate once already: the
# child of `systemd-run` is a *service*, so its standard output goes to the journal and the file this
# script redirects only ever holds systemd-run's own words. A gate that polls that redirected file for
# the listening line can never see it, however healthy the server is. The journal is what an operator
# reads, so it is what the gate reads.
if ! command -v journalctl >/dev/null 2>&1; then
	skip "no journalctl, so nothing can read back what a transient unit's server printed"
fi
mkdir -p "$STATE/store" "$STATE/tmp"
declare -a ENV_ARGS=()
while read -r line; do
	name=${line%%=*}
	value=${line#*=}
	# The two paths that name the machine's state directory are redirected into the scratch tree:
	# nothing here may write to /var/lib, and no gate here runs as root.
	case $name in
	WEFT_DATA_DIR) value=$STATE ;;
	WEFT_SEGMENT_STORE_ROOT) value="$STATE/store" ;;
	esac
	ENV_ARGS+=("--setenv=$name=$value")
done < <(sed -n 's|^Environment=||p' "$INSTALLED_UNIT")

# The state directory the unit asks systemd to create, read off the unit itself: `StateDirectory=w`
# means /var/lib/w, and that is the root `ReadWritePaths=` then names. A leading `-` (ignore
# failures) or `+` (persistent) is a flag on the directory, not part of its name.
unit_state=/var/lib/$(sed -n 's|^StateDirectory=||p' "$INSTALLED_UNIT" | head -1 | sed -E 's/^[-+]//; s/[[:space:]].*$//')
declare -a PROPS=()
while read -r line; do
	key=${line%%=*}
	value=${line#*=}
	case $key in
	# A transient service has no restart policy to honour, and `Environment=`/`ExecStart` are handled
	# above; everything else in [Service] is a security or lifetime property and is copied as-is.
	Restart | RestartSec | Environment | EnvironmentFile | ExecStart | WorkingDirectory) continue ;;
	# `ReadWritePaths=` names the deployment's state directory, which this gate neither owns nor
	# creates — `User=`, `Group=` and `StateDirectory=` are dropped below for the same reason. It
	# cannot simply be dropped though: with `ProtectSystem=strict` in force systemd builds the mount
	# namespace *before* it execs anything, and a read-write path that does not exist fails the start
	# outright. Measured on this host with the unit's own list: `(true)[…]: Failed to set up mount
	# namespacing: /var/lib/weftdb: No such file or directory`, `Failed at step NAMESPACE spawning`,
	# `status=226/NAMESPACE`. The path is where a deployment keeps its state, not a hardening choice,
	# so it is redirected into the scratch tree — the property, and what strict then means for
	# everything else in the hierarchy, stay under test.
	ReadWritePaths)
		redirected=
		for path in $value; do
			case $path in
			"$unit_state"*) path=$STATE ;;
			esac
			redirected+="$path "
		done
		line="ReadWritePaths=${redirected% }"
		;;
	esac
	PROPS+=("--property=$line")
done < <(awk '/^\[Service\]/{inside=1;next} /^\[/{inside=0} inside && /^[A-Z][A-Za-z]+=/' "$INSTALLED_UNIT")
# The counter is called `inside`, not `in`: `in` is a reserved word in awk grammar, which gawk
# tolerates as a variable name and Ubuntu's mawk refuses outright with a syntax error on the
# runner. A failed awk here yields zero properties and the gate below reports that, but the real
# fault was the program not parsing at all.
# The account and the state directory the unit names do not exist on the machine running this, and
# creating them is gate 6's business, not this one: the scope runs as the user who ran the test.
for drop in User Group StateDirectory; do
	for i in "${!PROPS[@]}"; do
		[[ ${PROPS[i]} == "--property=$drop="* ]] && unset 'PROPS[i]'
	done
done
readonly HARDEN_COUNT=${#PROPS[@]}
((HARDEN_COUNT >= 10)) || fail "only $HARDEN_COUNT [Service] hardening properties were read from the unit"

SCOPE=weft-host-install-$$
RUN_LOG="$WORK/run.out"
# `PrivateTmp=yes` under `ProtectSystem=strict` leaves the process no writable temporary directory
# of its own, and the store wants one: the unit's own comment records the rootless measurement where
# opening a store failed at migration 0001_baseline with `segment_index transaction failed with an
# error before COMMIT: BEGIN CONCURRENT: I/O error (tempdir): read-only filesystem or storage
# medium`, and names `Environment=TMPDIR=` inside `ReadWritePaths=` as the operator's answer. This
# gate takes that answer instead of switching the hardening off, so the property list under test is
# the one an operator ships.
# The scratch tree lives under $TMPDIR, which the sandbox hides: `PrivateTmp=yes` replaces /tmp and
# /var/tmp with a fresh tmpfs before `WorkingDirectory=` and `ReadWritePaths=` are resolved, and
# `ProtectHome=yes` masks /home, /root and /run/user — measured here, a scratch directory in either
# place fails with `Failed at step CHDIR … No such file or directory` and `status=200/CHDIR`, again
# with an empty child log. Neither property is negotiable (they are the point of the gate) and no
# portable machine has a /var/lib/weftdb this user owns, so the scratch directory is bound into the
# namespace instead. A deployment needs no such line — its state directory is a real one under
# /var/lib — and what the gate asks, whether the sandbox leaves the server somewhere to work, is
# unchanged by the plumbing.
STATE_BIND="BindPaths=$STATE $STATE"
# Which manager hosts the transient unit is not a detail the unit can control. The identical
# property list that starts cleanly under this host's systemd 262 user manager ends
# `Failed to drop capabilities: Operation not permitted`, `status=218/CAPABILITIES`, with an empty
# child log under the ubuntu-24.04 runner's user manager: an unprivileged manager has to build the
# unit's sandbox with fewer privileges than the unit assumes, and the first thing it cannot do is the
# capability drop that the rest of the restrictions sit on top of. That is a fact about the machine,
# not about `weft-server.service`, so the gate uses the stronger manager when one is reachable and
# names in its verdict which one ran the server: a pass from a user manager and a pass from the
# system manager are different claims, and the message must not let a reader trade them.
SUDO_ON=0
if command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then SUDO_ON=1; fi
# `systemctl` against the manager named first: the caller's own manager, or PID 1's, which is where an
# operator actually installs this unit.
ctl() {
	local mgr=$1
	shift
	if [[ $mgr == system ]]; then
		sudo -n systemctl "$@"
	else
		systemctl --user "$@"
	fi
}
# Reads back everything the transient unit's process wrote, which is the only account of a run that
# failed before or during exec.
unit_log() {
	local mgr=$1
	if [[ $mgr == system ]]; then
		sudo -n journalctl --no-pager -u "$SCOPE" 2>/dev/null
	else
		journalctl --user --no-pager -u "$SCOPE" 2>/dev/null
	fi
}
# Submits the transient unit with the property list read off the installed unit, under one manager.
submit() {
	local mgr=$1
	local -a run=()
	if [[ $mgr == system ]]; then
		run=(sudo -n systemd-run --quiet)
	else
		run=(systemd-run --user --quiet)
	fi
	"${run[@]}" --unit "$SCOPE" --collect \
		"${ENV_ARGS[@]}" "${PROPS[@]}" --property="$STATE_BIND" \
		--setenv=WEFT_SERVER_ADDR="127.0.0.1:0" \
		--setenv=WEFT_GPU_CALIBRATE=0 \
		--setenv=TMPDIR="$STATE/tmp" \
		--working-directory="$STATE" \
		"$BIN" serve >"$RUN_LOG" 2>&1
}
# A start that died in one of systemd's own setup steps — 200 CHDIR, 203 EXEC, 218 CAPABILITIES,
# 226 NAMESPACE, the 2xx band — never reached the server, so the failure belongs to the manager and
# trying a manager with more privileges is legitimate. Any other failure is the server's own
# behaviour, and retrying that somewhere privileged is precisely how a real defect gets buried.
preexec_step() {
	local mgr=$1 code status
	code=$(ctl "$mgr" show "$SCOPE" -p ExecMainCode --value 2>/dev/null)
	status=$(ctl "$mgr" show "$SCOPE" -p ExecMainStatus --value 2>/dev/null)
	if [[ $code == exited && $status =~ ^2[0-4][0-9]$ ]]; then printf 'status=%s' "$status"; fi
}
wait_up() {
	local mgr=$1 _
	for _ in $(seq 1 160); do
		unit_log "$mgr" | grep -q "listening on http://" && return 0
		ctl "$mgr" is-active --quiet "$SCOPE" || return 1
		sleep 0.25
	done
	return 1
}
MANAGER=
if systemctl --user is-active --quiet "user@$(id -u).service" 2>/dev/null ||
	systemctl --user show-environment >/dev/null 2>&1; then
	MANAGER=user
elif ((SUDO_ON)) && sudo -n systemctl is-system-running >/dev/null 2>&1; then
	MANAGER=system
else
	skip "no reachable user systemd manager, so the unit's hardening has never been loaded by anything"
fi

if [[ -n $MANAGER ]]; then
	verdict=
	step=
	if submit "$MANAGER"; then
		if wait_up "$MANAGER"; then verdict=ok; else verdict=not-listening; fi
	else
		verdict=submit-failed
		step=$(preexec_step "$MANAGER")
	fi
	if [[ $verdict == submit-failed && -n $step ]] && ((SUDO_ON)) && [[ $MANAGER == user ]] &&
		sudo -n systemctl is-system-running >/dev/null 2>&1; then
		ctl user stop "$SCOPE" >/dev/null 2>&1 || true
		MANAGER=system
		step=
		if submit system; then
			if wait_up system; then verdict=ok; else verdict=not-listening; fi
		else
			verdict=submit-failed
			step=$(preexec_step system)
		fi
	fi
	case $verdict in
	ok) ;;
	submit-failed)
		# Quote systemd's own account, not just the child's. A start that fails before the exec step —
		# sandbox or namespace setup — writes nothing to the run log and puts the reason in the journal,
		# which is how this gate first failed on a runner: an empty log and no clue. `226/NAMESPACE` and
		# `status=1/FAILURE` need different fixes, so the failure text has to say which one happened.
		blame=$(unit_log "$MANAGER" | tail -12)
		[[ -n $blame ]] || blame="the $MANAGER journal holds nothing for $SCOPE; systemd-run said:
$(tail -25 "$RUN_LOG")"
		ctl "$MANAGER" stop "$SCOPE" >/dev/null 2>&1 || true
		extra=
		if [[ -n $step ]]; then
			extra="
It failed inside systemd's own setup ($step), before the server was exec'd: that is usually the
$MANAGER manager missing a privilege rather than the unit being wrong. Rerun this script where the
system manager is reachable (sudo -n $SCRIPT_NAME --bin …), or on the deployment host, for the
stronger test."
		fi
		fail "the server would not start under the unit's own hardening (systemd manager: $MANAGER).$extra
systemd-run's own output (empty when systemd fails before it execs the binary):
$(tail -25 "$RUN_LOG")
systemd's account of the start:
$blame"
		;;
	not-listening)
		blame=$(unit_log "$MANAGER" | tail -25)
		[[ -n $blame ]] || blame="the $MANAGER journal holds nothing for $SCOPE; systemd-run said:
$(tail -5 "$RUN_LOG")"
		ctl "$MANAGER" stop "$SCOPE" >/dev/null 2>&1 || true
		fail "the server never reached its listening line under the unit's hardening (systemd manager: $MANAGER):
$blame"
		;;
	esac
	# `systemctl stop` sends the unit's KillSignal — SIGTERM here — and waits. A clean stop is the
	# whole claim of `TimeoutStopSec=90` and the graceful-shutdown path behind it.
	stop_rc=0
	ctl "$MANAGER" stop "$SCOPE" || stop_rc=$?
	if ((stop_rc != 0)); then
		fail "stopping the scope exited $stop_rc: SIGTERM did not take the server down inside the unit's own timeout"
	fi
	unit_log "$MANAGER" | grep -qE "shutdown|stopped|listening on" ||
		fail "the journal holds nothing about the run:
$(unit_log "$MANAGER" | tail -25)"
	pass "$HARDEN_COUNT hardening properties from the unit ran the real server under the $MANAGER systemd manager and stopped it with SIGTERM"
fi

echo
echo "$SCRIPT_NAME: PASS — a staged host install is the tree the manifest promises, and ${#PASSES[@]} gates hold"
if ((${#SKIPS[@]} > 0)); then
	echo "$SCRIPT_NAME: ${#SKIPS[@]} gate(s) were skipped on this machine:"
	for reason in "${SKIPS[@]}"; do echo "  - $reason"; done
fi
