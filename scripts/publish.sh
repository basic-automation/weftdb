#!/usr/bin/env bash
# Publish the WeftDB library crates to crates.io, in dependency order.
#
# Order matters: crates.io resolves path dependencies by version, so a crate cannot
# be published until everything it depends on is already on the registry.
#
#   ./scripts/publish.sh            # dry run: verifies every crate, uploads nothing
#   ./scripts/publish.sh --execute  # actually publishes
#
# Requires `cargo login` (or CARGO_REGISTRY_TOKEN in the environment). Run --execute
# from a checkout of the release tag, after the tag is pushed (RELEASING.md).
#
# Which crates this publishes, and in what order, is read from the manifests rather than
# copied into this file: `scripts/crates/check.py publish-order` prints every member the
# registry is allowed to carry, dependencies first. A new member therefore joins this run
# on its own, and a `publish = false` member — weft-server, weft-tui, weft-bench, which
# ship as GitHub release binaries — can never join it by accident. The same checker's
# `check` runs as the `crates` CI job, so the metadata the registry is about to be handed
# is asserted on the PR instead of at the tag.
set -euo pipefail

cd "$(dirname "$0")/.."

CHECK="scripts/crates/check.py"

# Better to stop here than halfway through a partial release: a crate whose version
# requirement is out of step with the member it names uploads, and then cannot be resolved.
if ! python3 "$CHECK" check; then
	echo "refusing to publish: the per-crate release metadata is out of step (the lines above name it)" >&2
	exit 1
fi

# `while read`, not `mapfile`: the operator's shell may be the bash 3.2 that macOS ships.
CRATES=()
while IFS= read -r crate; do
	CRATES+=("$crate")
done < <(python3 "$CHECK" publish-order)
if [[ ${#CRATES[@]} -eq 0 ]]; then
	echo "no member of this workspace is publishable, which is not a library release" >&2
	exit 1
fi

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

# Each crate carries its own version (RELEASING.md), so a patch release leaves most of them
# at a version crates.io already has. Skip those instead of letting `cargo publish` fail
# on the first one and abort the run.
crate_version() {
	cargo metadata --format-version 1 --no-deps |
		python3 -c 'import json,sys; n=sys.argv[1]; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]==n))' "$1"
}
on_registry() {
	local code
	code=$(curl -s -o /dev/null -w '%{http_code}' -A "weftdb-publish-script" "https://crates.io/api/v1/crates/$1/$2")
	[[ "$code" == "200" ]]
}

for crate in "${CRATES[@]}"; do
	version=$(crate_version "$crate")
	if on_registry "$crate" "$version"; then
		echo "==> skipping $crate $version: already on crates.io"
		continue
	fi
	echo "==> publishing $crate $version"
	cargo publish -p "$crate"
	# The registry index needs a moment before the next crate can resolve this one.
	echo "    waiting for the index to catch up..."
	sleep 30
done

echo
echo "All library crates published."
echo "Next: check and publish the draft GitHub release the release workflow made for"
echo "this tag (RELEASING.md, \"Publish the GitHub release\")."
