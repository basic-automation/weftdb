#!/usr/bin/env bash
# The R client's tests (clients/r): the offline checks, then the live contract against a
# weft-server started here on a free loopback port over a throwaway store.
#
#   scripts/clients/test-r.sh target/debug/weft-server
#
# R runs in a pinned rocker/r-ver container (no R on the host needed), with arrow, curl,
# jsonlite and testthat from Posit Package Manager's binaries, and reaches the server over
# the host network. Docker is the only requirement.
set -euo pipefail

server_bin=${1:?usage: test-r.sh <path to weft-server>}
image='rocker/r-ver:4.4.2'
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
server_bin=$(cd -- "$(dirname -- "$server_bin")" && pwd)/$(basename -- "$server_bin")
[[ -x $server_bin ]] || { echo "test-r.sh: $server_bin is not an executable" >&2; exit 2; }

work=$(mktemp -d)
server_pid=''
cleanup() {
	if [[ -n $server_pid ]]; then
		kill "$server_pid" 2>/dev/null || true
		wait "$server_pid" 2>/dev/null || true
	fi
	rm -rf "$work"
}
trap cleanup EXIT

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
env -i PATH="$PATH" HOME="$work" WEFT_SERVER_ADDR="127.0.0.1:$port" WEFT_SEGMENT_STORE_ROOT="$work/store" \
	WEFT_GPU_CALIBRATE=0 RUST_LOG=warn "$server_bin" >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 300); do
	if curl -sf "http://127.0.0.1:$port/health" >/dev/null; then break; fi
	if ! kill -0 "$server_pid" 2>/dev/null; then cat "$work/server.log" >&2; exit 1; fi
	sleep 0.2
done
curl -sf "http://127.0.0.1:$port/health" >/dev/null || { echo "weft-server did not come up:" >&2; cat "$work/server.log" >&2; exit 1; }

docker run --rm --network host \
	-v "$root/clients/r:/pkg:ro" \
	-e "WEFT_SERVER_URL=http://127.0.0.1:$port" \
	"$image" bash -euo pipefail -c '
		Rscript -e "options(Ncpus = 4, timeout = 600); pkgs <- c(\"arrow\", \"bit64\", \"curl\", \"jsonlite\", \"testthat\"); install.packages(pkgs, quiet = TRUE); missing <- setdiff(pkgs, rownames(installed.packages())); if (length(missing)) install.packages(missing); stopifnot(all(pkgs %in% rownames(installed.packages())))"
		cp -r /pkg /tmp/pkg
		R CMD INSTALL /tmp/pkg
		cd /tmp/pkg/tests
		Rscript -e "library(testthat); library(weftdb); test_dir(\"testthat\", reporter = \"summary\", stop_on_failure = TRUE)"
	'
