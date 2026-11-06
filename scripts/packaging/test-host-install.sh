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
#       Skipped, loudly, where systemd is not installed.
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
# invalid configuration before anything binds, so the server cannot serve even by accident
# and the only refusal a gate can be reacting to names this variable.
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
# gate 4 is that the env file cannot carry one, and the only way to know that is to try.
TEST_NAME=$(grep -B3 'tier: Tier::Test' "$SETTINGS" |
	grep -oE '"(WEFT|OTEL)_[A-Z0-9_]+"' | tr -d '"' | head -1) || true
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
	verify_out=$(systemd-analyze verify --root "$DEST" \
		"${PREFIX#/}/lib/systemd/system/weft-server.service" 2>&1) && verify_rc=0 || verify_rc=$?
	[[ $verify_rc -eq 0 ]] ||
		fail "systemd-analyze verify rejected the installed unit (exit $verify_rc):
$verify_out"
	pass "systemd-analyze verify is silent about the installed unit"
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

declare -a PROPS=()
while read -r line; do
	key=${line%%=*}
	case $key in
	# A transient scope has no restart policy to honour, and `Environment=`/`ExecStart` are handled
	# above; everything else in [Service] is a security or lifetime property and is copied as-is.
	Restart | RestartSec | Environment | EnvironmentFile | ExecStart | WorkingDirectory) continue ;;
	esac
	PROPS+=("--property=$line")
done < <(awk '/^\[Service\]/{in=1;next} /^\[/{in=0} in && /^[A-Z][A-Za-z]+=/' "$INSTALLED_UNIT")
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
if ! systemctl --user is-active --quiet "user@$(id -u).service" 2>/dev/null &&
	! systemctl --user show-environment >/dev/null 2>&1; then
	skip "no reachable user systemd manager, so the unit's hardening has never been loaded by anything"
elif ! systemd-run --user --quiet --unit "$SCOPE" --collect \
	"${ENV_ARGS[@]}" "${PROPS[@]}" \
	--setenv=WEFT_SERVER_ADDR="127.0.0.1:0" \
	--setenv=WEFT_GPU_CALIBRATE=0 \
	--working-directory="$STATE" \
	"$BIN" serve >"$RUN_LOG" 2>&1; then
	systemctl --user stop "$SCOPE" >/dev/null 2>&1 || true
	fail "the server would not start under the unit's own hardening:
$(tail -25 "$RUN_LOG")"
else
	up=0
	for _ in $(seq 1 40); do
		grep -q "listening on http://" "$RUN_LOG" && {
			up=1
			break
		}
		systemctl --user is-active --quiet "$SCOPE" || break
		sleep 0.25
	done
	if ((up == 0)); then
		systemctl --user stop "$SCOPE" >/dev/null 2>&1 || true
		fail "the server never reached its listening line under the unit's hardening:
$(tail -25 "$RUN_LOG")"
	fi
	# `systemctl stop` sends the unit's KillSignal — SIGTERM here — and waits. A clean stop is the
	# whole claim of `TimeoutStopSec=90` and the graceful-shutdown path behind it.
	stop_rc=0
	systemctl --user stop "$SCOPE" || stop_rc=$?
	if ((stop_rc != 0)); then
		fail "stopping the scope exited $stop_rc: SIGTERM did not take the server down inside the unit's own timeout"
	fi
	grep -qE "shutdown|stopped|listening on" "$RUN_LOG" ||
		fail "the run log holds nothing about the shutdown:
$(tail -25 "$RUN_LOG")"
	pass "$HARDEN_COUNT hardening properties from the unit ran the real server and stopped it with SIGTERM"
fi

echo
echo "$SCRIPT_NAME: PASS — a staged host install is the tree the manifest promises, and ${#PASSES[@]} gates hold"
if ((${#SKIPS[@]} > 0)); then
	echo "$SCRIPT_NAME: ${#SKIPS[@]} gate(s) were skipped on this machine:"
	for reason in "${SKIPS[@]}"; do echo "  - $reason"; done
fi
