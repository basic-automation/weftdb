#!/usr/bin/env bash
#
# install-host.sh — install WeftDB the way a machine that is not a container is installed.
#
# PAC-11 (1.0 plan): systemd unit, user and env files, and host-install CI. This is the
# install half; `scripts/packaging/test-host-install.sh` is the gate that proves the result.
#
# Usage: scripts/packaging/install-host.sh [--prefix DIR] [--destdir DIR] [--bin PATH] [--list]
#   --prefix DIR   where the files live once the machine is running them. Default /usr/local,
#                  which is what `scripts/release/stage.sh` builds the release archives against
#                  and what the unit documents as `/usr/local/bin/weft-server`. /usr is a
#                  distribution's territory and this project is not one.
#   --destdir DIR  stage into DIR instead of the live filesystem: the same tree, rooted at DIR.
#                  This is what packaging and CI use, and what a package manager's %install does.
#   --bin PATH     the weft-server binary to install. Default the first of
#                  $WEFT_BIN, target/release/weft-server, target/debug/weft-server that exists.
#   --list         print the file manifest (source -> destination, one per line) and exit 0
#                  without copying anything.
#   -h, --help     this text.
#
# What it installs, and where:
#
#   <prefix>/bin/weft-server                          0755  the server
#   <prefix>/lib/systemd/system/weft-server.service   0644  the unit, with ExecStart rewritten
#   <prefix>/lib/sysusers.d/weftdb.conf               0644  the service account (PAC-11)
#   <prefix>/lib/tmpfiles.d/weftdb.conf               0644  /run/weftdb, where keys go
#   <prefix>/etc/weftdb/weft-server.env               0640  created once, never overwritten
#   <prefix>/share/doc/weftdb/*                       0644  the env template and the licence bundle
#
# Three things this script deliberately does not do.
#
# It does not touch the account, the unit or the running system. It places files. The operator
# (or a package manager's scriptlet) runs `systemd-sysusers`, `systemd-tmpfiles --create`,
# `systemctl daemon-reload` and `systemctl enable --now weft-server`, and the script prints
# those four commands at the end so nobody has to look them up. Installing a unit is the point
# at which a machine starts running a daemon, and an install script that did that silently —
# from a tarball, in a CI job, on the way to a `make install` — is a machine that starts
# running daemons silently.
#
# It does not overwrite an operator's configuration. `<prefix>/etc/weftdb/weft-server.env` is
# the operator's file; if it exists the script leaves it alone and says so, because on the next
# upgrade the alternative is losing whatever they set. The copy in `share/doc` is the template,
# always refreshed, and the diff between the two is the operator's changes.
#
# It does not create the data directory. `StateDirectory=weftdb` in the unit does that at first
# start, owned by the service user and mode 0755 minus the umask; `weft-server` creates
# `WEFT_DATA_DIR` itself if it is pointed somewhere else. A directory made here would be made
# as root with the wrong owner either way.
#
# Exit status: 0 on success, 2 on a usage error, 3 on a missing source file.

set -euo pipefail

readonly SCRIPT_NAME="${0##*/}"
readonly REPO_ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)

PREFIX=/usr/local
DESTDIR=
BIN=${WEFT_BIN:-}
LIST_ONLY=0

die() {
	echo "$SCRIPT_NAME: $1" >&2
	[[ ${2:-} == usage ]] && usage >&2
	exit "${3:-1}"
}

usage() {
	sed -n '3,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

# The manifest: one "source<TAB>destination-mode" per line, relative to the prefix. Both the
# installer and `--list` read this, and so does the test script, so the list of what a host
# install consists of lives in exactly one place.
manifest() {
	printf '%s\t%s\t%s\n' "$BIN" "$PREFIX/bin/weft-server" 0755
	printf '%s\t%s\t%s\n' "$REPO_ROOT/packaging/systemd/weft-server.service" \
		"$PREFIX/lib/systemd/system/weft-server.service" 0644
	printf '%s\t%s\t%s\n' "$REPO_ROOT/packaging/systemd/weftdb.sysusers" \
		"$PREFIX/lib/sysusers.d/weftdb.conf" 0644
	printf '%s\t%s\t%s\n' "$REPO_ROOT/packaging/systemd/weftdb.tmpfiles" \
		"$PREFIX/lib/tmpfiles.d/weftdb.conf" 0644
	printf '%s\t%s\t%s\n' "$REPO_ROOT/packaging/env/weft-server.env" \
		"$PREFIX/share/doc/weftdb/weft-server.env.example" 0644
	local doc
	for doc in LICENSE-APACHE LICENSE-MIT NOTICE THIRD-PARTY-LICENSES.md; do
		printf '%s\t%s\t%s\n' "$REPO_ROOT/$doc" "$PREFIX/share/doc/weftdb/$doc" 0644
	done
}

while (($# > 0)); do
	case $1 in
	--prefix)
		[[ $# -ge 2 ]] || die "--prefix needs a value" usage 2
		PREFIX=${2%/}
		[[ $PREFIX == /* ]] || die "--prefix must be an absolute path, got $PREFIX" usage 2
		shift 2
		;;
	--destdir)
		[[ $# -ge 2 ]] || die "--destdir needs a value" usage 2
		DESTDIR=${2%/}
		[[ $DESTDIR == /* ]] || die "--destdir must be an absolute path, got $DESTDIR" usage 2
		shift 2
		;;
	--bin)
		[[ $# -ge 2 ]] || die "--bin needs a value" usage 2
		BIN=$2
		shift 2
		;;
	--list)
		LIST_ONLY=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		die "unknown argument: $1" usage 2
		;;
	esac
done

# The binary: --bin, then $WEFT_BIN, then the release build, then the debug build. A host install
# of a debug binary is a legitimate thing to want (it is how a unit gets tested in CI without
# waiting for an LTO build), so the fallback order is documented rather than strict.
if [[ -z $BIN ]]; then
	for candidate in "$REPO_ROOT/target/release/weft-server" "$REPO_ROOT/target/debug/weft-server"; do
		[[ -x $candidate ]] && {
			BIN=$candidate
			break
		}
	done
fi
[[ -n $BIN ]] ||
	die "no weft-server binary to install: build one, or pass --bin PATH / WEFT_BIN=PATH" "" 3
[[ -x $BIN ]] || die "$BIN is not an executable file" "" 3

if ((LIST_ONLY)); then
	manifest
	exit 0
fi

# Every source has to exist before anything is copied: an install that fails halfway leaves a
# machine with a unit file and no binary, which is worse than no install at all.
while IFS=$'\t' read -r src _dst _mode; do
	[[ -e $src ]] || die "missing $src: this script must run from a checkout of the repository" "" 3
done < <(manifest)

# Install one file with an explicit mode. `install -m` does both at once; it is spelled out here
# because a file that lands world-readable because the umask said so is the bug this avoids.
install_file() {
	local src=$1 dst=$2 mode=$3
	install -D -m "$mode" "$src" "$DESTDIR$dst"
}

while IFS=$'\t' read -r src dst mode; do
	install_file "$src" "$dst" "$mode"
done < <(manifest)

# The unit's ExecStart names an absolute path, and that path has to be this install's, so the
# packaged copy is rewritten on the way in rather than carrying a path that happens to be right
# for a tarball. `--prefix` is the only thing that changes it.
readonly UNIT="$PREFIX/lib/systemd/system/weft-server.service"
sed -e "s|^ExecStart=.*/bin/weft-server|ExecStart=$PREFIX/bin/weft-server|" \
	"$DESTDIR$UNIT" >"$DESTDIR$UNIT.tmp" &&
	mv "$DESTDIR$UNIT.tmp" "$DESTDIR$UNIT"

# The operator's env file: created once from the template, never replaced. Mode 0640 because the
# file is where an operator who does not use a secrets manager will end up pointing WEFT_KEYS_FILE
# — and the template itself is commented out, so a fresh copy grants nothing.
readonly OPERATOR_ENV="$PREFIX/etc/weftdb/weft-server.env"
if [[ -e $DESTDIR$OPERATOR_ENV ]]; then
	echo "$SCRIPT_NAME: left $DESTDIR$OPERATOR_ENV alone (your configuration; the template is at $PREFIX/share/doc/weftdb/weft-server.env.example)"
else
	install_file "$REPO_ROOT/packaging/env/weft-server.env" "$OPERATOR_ENV" 0640
fi

cat <<-EOF
	$SCRIPT_NAME: installed into $DESTDIR$PREFIX

	Nothing has started. To finish, as root:

	  systemd-sysusers                 # creates the weftdb account and /var/lib/weftdb
	  systemd-tmpfiles --create        # creates /run/weftdb, where a keys file goes
	  systemctl daemon-reload          # picks up $UNIT
	  systemctl enable --now weft-server

	Then read what it did:

	  systemctl status weft-server
	  journalctl -u weft-server --no-pager -n 50

	To change a setting, edit $OPERATOR_ENV and run systemctl restart weft-server.
	EOF
