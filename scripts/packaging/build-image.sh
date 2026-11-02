#!/usr/bin/env bash
#
# build-image.sh — build the WeftDB server container image (1.0 plan PAC-9).
#
# One wrapper around `docker build` for three jobs it does better than a bare command line:
#
#   * it pins nothing itself — every image reference and every toolchain version comes from
#     packaging/docker/Dockerfile and .github/release-toolchain, so this script cannot drift
#     away from the release pipeline;
#   * it stamps the OCI source labels (version, revision, created) from the checkout being
#     built, so a running container says which commit it came from;
#   * it fails on a daemon it cannot reach or a context that is missing its Dockerfile, before
#     paying for a build.
#
# Usage:
#   scripts/packaging/build-image.sh [--tag NAME[:TAG]] [--docker COMMAND] [--smoke]
#                                    [--builder-image IMAGE] [--runtime-image IMAGE]
#                                    [--no-labels] [--] [extra docker build arguments...]
#
# --tag NAME[:TAG]        the tag to apply. Default: weftdb:git-<12 hex of HEAD>, or
#                         weftdb:dirty-<12 hex> with a dirty tree, so a container built from
#                         uncommitted sources cannot be mistaken for one built from a commit.
# --docker COMMAND        the container CLI to drive (default: docker). Set it to a
#                         podman-docker shim, or to "docker --context foo".
# --smoke                 after building, run scripts/packaging/smoke-container.sh against the
#                         tag. This is the pair CI runs.
# --builder-image IMAGE   override the build baseline. Only for testing a newer baseline before
#                         the pin in the Dockerfile is changed; a successful build with this
#                         flag says nothing about the published glibc floor.
# --runtime-image IMAGE   override the runtime base, likewise.
# --no-labels             build with no version/revision/created labels, for a byte-reproducible
#                         comparison (the labels are the only field that differs per build).
#
# Single architecture: the builder compiles for the architecture it runs on, so the image matches
# the host. Multi-arch is PAC-10, along with the registry decision.
#
# Exit status: 0 the image exists under the requested tag; 1 the build or the smoke test failed;
# 2 bad usage or no usable container CLI.

set -euo pipefail
# Byte-wise comparisons and `[0-9a-f]` matching: in a UTF-8 locale bash's ranges match other
# scripts' digits, the same reason scripts/release/stage.sh sets this.
export LC_ALL=C

SCRIPT_NAME=${0##*/}
DOCKER=docker
TAG=""
SMOKE=0
BUILDER_IMAGE=""
RUNTIME_IMAGE=""
LABELS=1
EXTRA_ARGS=()

fail() {
	echo "$SCRIPT_NAME: $*" >&2
	exit 1
}

usage() {
	sed -n '3,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

while (($# > 0)); do
	case $1 in
	--tag)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --tag needs a value" >&2
			exit 2
		}
		TAG=$2
		shift 2
		;;
	--docker)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --docker needs a value" >&2
			exit 2
		}
		DOCKER=$2
		shift 2
		;;
	--smoke)
		SMOKE=1
		shift
		;;
	--builder-image)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --builder-image needs a value" >&2
			exit 2
		}
		BUILDER_IMAGE=$2
		shift 2
		;;
	--runtime-image)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --runtime-image needs a value" >&2
			exit 2
		}
		RUNTIME_IMAGE=$2
		shift 2
		;;
	--no-labels)
		LABELS=0
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	--)
		shift
		EXTRA_ARGS=("$@")
		break
		;;
	*)
		echo "$SCRIPT_NAME: there is no \`$1\` flag; \`$SCRIPT_NAME --help\` lists what this takes" >&2
		exit 2
		;;
	esac
done

# The repository root, so the script works from any directory and the context is the tree the
# Dockerfile's `COPY . .` expects.
ROOT=$(git rev-parse --show-toplevel 2>/dev/null) ||
	fail "run this from inside the weftdb repository; it needs a git checkout to take its labels from"
DOCKERFILE=$ROOT/packaging/docker/Dockerfile
[[ -f $DOCKERFILE ]] || fail "$DOCKERFILE is missing; the image definition lives with the packaging"

# A daemon that is up but unreachable costs a confusing build failure, so ask first.
# shellcheck disable=SC2086 # --docker is intentionally word-split ("docker --context foo").
if ! $DOCKER info >/dev/null 2>&1; then
	echo "$SCRIPT_NAME: \`$DOCKER info\` failed: no container daemon is reachable." >&2
	echo "$SCRIPT_NAME: start the daemon, or point --docker at one (e.g. --docker podman)." >&2
	exit 2
fi

# ---------------------------------------------------------------------------
# the tag and the source labels, read out of the checkout
# ---------------------------------------------------------------------------
if ! HEAD=$(git -C "$ROOT" rev-parse HEAD 2>/dev/null); then
	fail "cannot read HEAD of $ROOT"
fi
SHORT=${HEAD:0:12}
if [[ -z $TAG ]]; then
	# A dirty tree is tagged by its content, not by a commit that does not describe it.
	if [[ -n $(git -C "$ROOT" status --porcelain) ]]; then
		TAG=weftdb:dirty-$SHORT
	else
		TAG=weftdb:git-$SHORT
	fi
fi
# A tag is a name:tag pair, and a name must not look like a path.
[[ $TAG =~ ^[A-Za-z0-9._-]+(/[A-Za-z0-9._-]+)*(:[A-Za-z0-9._-]+)?$ ]] ||
	fail "--tag must be NAME or NAME:TAG with no slashes at either end and no @digest; got \`$TAG\`"

BUILD_ARGS=()
if ((LABELS)); then
	# The version is what a release would call this tree: a tag when the checkout is one, and
	# the commit otherwise. `git describe` is the same thing scripts/release/verify.sh resolves a
	# release from, so a container and an archive built from one commit agree on the string.
	VERSION=$(git -C "$ROOT" describe --tags --always --dirty 2>/dev/null || echo "$SHORT")
	# The created timestamp is the commit's, not the build clock's: two builds of one commit then
	# differ only where they genuinely differ, which is what makes the image diffable in a review.
	CREATED=$(git -C "$ROOT" log -1 --pretty=%cI HEAD)
	BUILD_ARGS+=(--build-arg "VERSION=$VERSION" --build-arg "REVISION=$HEAD" --build-arg "CREATED=$CREATED")
fi
[[ -n $BUILDER_IMAGE ]] && BUILD_ARGS+=(--build-arg "BUILDER_IMAGE=$BUILDER_IMAGE")
[[ -n $RUNTIME_IMAGE ]] && BUILD_ARGS+=(--build-arg "RUNTIME_IMAGE=$RUNTIME_IMAGE")

echo "$SCRIPT_NAME: building $TAG from ${VERSION:-$HEAD} (labels: $([[ $LABELS == 1 ]] && echo yes || echo no))"

# ---------------------------------------------------------------------------
# build
# ---------------------------------------------------------------------------
# BuildKit is the default engine on a modern Docker, and the Dockerfile's cache mounts need it;
# exported explicitly so an older daemon with the legacy builder fails here rather than
# mid-COPY.
if ! DOCKER_BUILDKIT=1 $DOCKER build \
	--file "$DOCKERFILE" \
	--tag "$TAG" \
	--label "org.opencontainers.image.base.name=weftdb-packaging" \
	"${BUILD_ARGS[@]}" \
	${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
	"$ROOT"; then
	fail "the image build failed; the output above is the reason"
fi

# Read the result back out of the daemon rather than trusting the build's exit status alone: a
# tag that exists but points somewhere else is the failure mode this catches.
if ! ID=$($DOCKER image inspect --format '{{.Id}}' "$TAG" 2>/dev/null); then
	fail "the build reported success but no image is tagged \`$TAG\`"
fi
SIZE=$($DOCKER image inspect --format '{{.Size}}' "$TAG")
echo "$SCRIPT_NAME: built $TAG ($ID, $((SIZE / 1024 / 1024)) MiB)"

if ((SMOKE)); then
	exec "$ROOT/scripts/packaging/smoke-container.sh" --image "$TAG" --docker "$DOCKER"
fi

exit 0
