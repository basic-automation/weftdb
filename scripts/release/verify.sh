#!/usr/bin/env bash
# Decide whether the release workflow may build a commit, and which commit that is.
#
#   scripts/release/verify.sh tag <vX.Y.Z | vX.Y.Z-rc.N>   # a release
#   scripts/release/verify.sh ref <branch | tag | commit>   # a dry run
#
# A release fails unless every one of these holds:
#   1. the tag is exactly vX.Y.Z or vX.Y.Z-rc.N (it is checked before any other use);
#   2. refs/tags/<tag> already exists, so the release can never create it;
#   3. <tag> is "v" followed by weft-server's version at that commit (cargo metadata);
#   4. the commit is an ancestor of origin/main, or of an origin/release/* branch (for
#      patch releases cut from a release branch);
#   5. the most recent completed run of the CI workflow for the commit concluded success
#      (cancelled and skipped runs are ignored);
#   6. CHANGELOG.md at the commit has a non-empty "## [<version>]" section.
# A dry run checks only 4 and 5, against <ref>.
#
# On success it prints, and appends to $GITHUB_OUTPUT when that is set:
#   sha=<commit>  version=<weft-server version>  tag=<tag>  prerelease=<true|false>
# For a dry run, tag is the tag this version would be released as (v<version>).
#
# It reads the repository it runs in, which must have every branch and tag fetched
# (actions/checkout with fetch-depth: 0). It needs git, cargo, jq and gh.
#
# Environment:
#   GITHUB_REPOSITORY  owner/name whose CI runs are checked (required)
#   GH_TOKEN           token for gh, with actions: read
#   CI_WORKFLOW        the CI workflow file (default: ci.yml)
#   NOTES_OUT          a release writes its CHANGELOG section here (optional)
#   GITHUB_OUTPUT      receives the outputs above (optional)
set -euo pipefail

TAG_RE='^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.(0|[1-9][0-9]*))?$'
# Branch and tag names as this repository uses them, or an abbreviated or full commit id.
REF_RE='^[A-Za-z0-9][A-Za-z0-9._/-]{0,199}$'
VERSION_RE='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'

fail() {
	if [[ ${GITHUB_ACTIONS:-} == true ]]; then
		printf '::error title=release verify::%s\n' "$*"
	else
		printf 'verify: %s\n' "$*" >&2
	fi
	exit 1
}

note() { printf 'verify: %s\n' "$*"; }

emit() {
	printf '%s=%s\n' "$1" "$2"
	if [[ -n ${GITHUB_OUTPUT:-} ]]; then
		printf '%s=%s\n' "$1" "$2" >>"$GITHUB_OUTPUT"
	fi
}

usage() { fail "usage: verify.sh tag <vX.Y.Z|vX.Y.Z-rc.N> | verify.sh ref <ref>"; }

# Sets $version to weft-server's version at a commit. cargo metadata reads it from a
# scratch worktree, so the caller's checkout is never touched.
read_version() {
	local sha=$1
	git worktree add --detach --quiet "$scratch/src" "$sha" >/dev/null 2>&1 ||
		fail "could not check out $sha to read its version"
	version=$(cargo metadata --no-deps --format-version 1 --manifest-path "$scratch/src/Cargo.toml" |
		jq -r '[.packages[] | select(.name == "weft-server") | .version] | first // empty') ||
		fail "cargo metadata failed at $sha"
	[[ -n $version ]] || fail "no weft-server package at $sha"
	[[ $version =~ $VERSION_RE ]] || fail "weft-server's version at $sha is not semver: $version"
}

resolve_ref() {
	local ref=$1 candidate sha
	for candidate in "refs/remotes/origin/$ref" "refs/tags/$ref" "refs/heads/$ref"; do
		if sha=$(git rev-parse --verify --quiet "$candidate^{commit}"); then
			printf '%s\n' "$sha"
			return 0
		fi
	done
	if [[ $ref =~ ^[0-9a-f]{7,40}$ ]] && sha=$(git rev-parse --verify --quiet "$ref^{commit}"); then
		printf '%s\n' "$sha"
		return 0
	fi
	return 1
}

check_ancestry() {
	local sha=$1 branch
	local -a branches=(refs/remotes/origin/main)
	while IFS= read -r branch; do
		branches+=("$branch")
	done < <(git for-each-ref --format='%(refname)' refs/remotes/origin/release/)
	for branch in "${branches[@]}"; do
		if git rev-parse --verify --quiet "$branch^{commit}" >/dev/null &&
			git merge-base --is-ancestor "$sha" "$branch"; then
			note "ancestry: $sha is on ${branch#refs/remotes/}"
			return 0
		fi
	done
	fail "$sha is not an ancestor of origin/main or of any origin/release/* branch"
}

check_ci() {
	local sha=$1 workflow=${CI_WORKFLOW:-ci.yml} runs latest conclusion url
	[[ -n ${GITHUB_REPOSITORY:-} ]] || fail "GITHUB_REPOSITORY is not set"
	runs=$(gh api --method GET "repos/$GITHUB_REPOSITORY/actions/workflows/$workflow/runs" \
		-f head_sha="$sha" -f per_page=100) ||
		fail "could not list $workflow runs for $sha"
	latest=$(jq -r --arg sha "$sha" '
		[.workflow_runs[]
			| select(.head_sha == $sha and .status == "completed")
			| select(.conclusion != "cancelled" and .conclusion != "skipped")]
		| sort_by(.created_at) | last
		| if . == null then "none -" else "\(.conclusion) \(.html_url)" end' <<<"$runs") ||
		fail "could not read the $workflow runs for $sha"
	conclusion=${latest%% *}
	url=${latest#* }
	case $conclusion in
	success) note "ci: $workflow passed for $sha ($url)" ;;
	none) fail "$workflow has no completed run for $sha; wait for CI, or run it" ;;
	*) fail "$workflow's latest run for $sha concluded $conclusion ($url)" ;;
	esac
}

check_changelog() {
	local sha=$1 version=$2 changelog notes
	changelog=$(git show "$sha:CHANGELOG.md" 2>/dev/null) || fail "no CHANGELOG.md at $sha"
	# The section runs from its heading to the next "## [" heading or the link
	# definitions at the foot of the file.
	notes=$(awk -v heading="## [$version]" '
		index($0, heading) == 1 { found = 1; next }
		found && (/^## \[/ || /^\[[^ ]*\]: /) { exit }
		found { print }
	' <<<"$changelog")
	grep -q '[^[:space:]]' <<<"$notes" ||
		fail "CHANGELOG.md at $sha has no non-empty '## [$version]' section"
	note "changelog: found the [$version] section"
	if [[ -n ${NOTES_OUT:-} ]]; then
		printf '%s\n' "$notes" >"$NOTES_OUT"
	fi
}

[[ $# -eq 2 ]] || usage
mode=$1
arg=$2

scratch=$(mktemp -d)
cleanup() {
	if [[ -d $scratch/src ]]; then
		git worktree remove --force "$scratch/src" >/dev/null 2>&1 || true
	fi
	rm -rf "$scratch"
}
trap cleanup EXIT

case $mode in
tag)
	tag=$arg
	[[ $tag =~ $TAG_RE ]] || fail "tag must be vX.Y.Z or vX.Y.Z-rc.N; got $(printf '%q' "$tag")"
	sha=$(git rev-parse --verify --quiet "refs/tags/$tag^{commit}") ||
		fail "tag $tag does not exist; push it first (a release never creates its tag)"
	note "tag: $tag is $sha"
	read_version "$sha"
	[[ $tag == "v$version" ]] || fail "tag $tag does not match weft-server's version $version at $sha"
	check_ancestry "$sha"
	check_changelog "$sha" "$version"
	check_ci "$sha"
	;;
ref)
	ref=$arg
	[[ $ref =~ $REF_RE && $ref != *..* ]] || fail "not a usable ref: $(printf '%q' "$ref")"
	sha=$(resolve_ref "$ref") || fail "ref $ref does not resolve to a commit"
	note "ref: $ref is $sha (dry run: ancestry and CI only)"
	read_version "$sha"
	tag="v$version"
	check_ancestry "$sha"
	check_ci "$sha"
	;;
*) usage ;;
esac

prerelease=false
[[ $tag == *-* ]] && prerelease=true

emit sha "$sha"
emit version "$version"
emit tag "$tag"
emit prerelease "$prerelease"
