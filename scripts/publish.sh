#!/usr/bin/env bash
# Publish the WeftDB library crates to crates.io, in dependency order.
#
# Order matters: crates.io resolves path dependencies by version, so a crate cannot
# be published until everything it depends on is already on the registry.
#
#   ./scripts/publish.sh            # dry run: verifies every crate, uploads nothing
#   ./scripts/publish.sh --execute  # actually publishes
#
# Requires `cargo login` (or CARGO_REGISTRY_TOKEN in the environment).
#
# weft-server, weft-tui and weft-bench are `publish = false`: they ship as GitHub
# release binaries, not registry crates.
set -euo pipefail

cd "$(dirname "$0")/.."

# Dependency order, leaves first.
CRATES=(
	splimes
	weft-physical-type
	weft-reduce
	weft-line-protocol
	weft-arrow
	weftdb
	weft-arrow-store
	weft-orchestration
)

EXECUTE=0
[[ "${1:-}" == "--execute" ]] && EXECUTE=1

if [[ $EXECUTE -eq 0 ]]; then
	echo "DRY RUN — nothing will be uploaded. Re-run with --execute to publish."
	echo
	# A dry run cannot see crates published earlier in this same run, so anything
	# with an unpublished path dependency will fail to resolve. Verify each crate
	# on its own and let the operator read the failures in that light.
	for crate in "${CRATES[@]}"; do
		echo "==> cargo publish --dry-run -p $crate"
		cargo publish --dry-run -p "$crate" || echo "    (expected while its dependencies are not yet on crates.io)"
	done
	exit 0
fi

for crate in "${CRATES[@]}"; do
	echo "==> publishing $crate"
	cargo publish -p "$crate"
	# The registry index needs a moment before the next crate can resolve this one.
	echo "    waiting for the index to catch up..."
	sleep 30
done

echo
echo "All library crates published."
echo "Next: push the tag so the release workflow builds the binaries, e.g."
echo "  git tag -a v0.1.0 -m 'WeftDB v0.1.0' && git push origin v0.1.0"
