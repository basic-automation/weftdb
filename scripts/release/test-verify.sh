#!/usr/bin/env bash
# Tests for scripts/release/verify.sh, against a throwaway git repository and a stubbed
# gh. Needs git, cargo and jq; touches nothing outside a temp directory.
#
#   scripts/release/test-verify.sh
set -euo pipefail

verify="$(cd "$(dirname "$0")" && pwd)/verify.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

repo=$tmp/repo
bin=$tmp/bin
runs=$tmp/runs.json
mkdir -p "$bin"

# gh stub: answers `gh api --method GET repos/<repo>/actions/workflows/ci.yml/runs
# -f head_sha=<sha> ...` from $runs, filtered by head_sha as the API filters it.
cat >"$bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == api ]] || { echo "gh stub: unexpected command: $*" >&2; exit 2; }
sha='' path=''
for arg in "$@"; do
	case $arg in
	head_sha=*) sha=${arg#head_sha=} ;;
	repos/*) path=$arg ;;
	esac
done
[[ $path == "repos/$GITHUB_REPOSITORY/actions/workflows/ci.yml/runs" ]] ||
	{ echo "gh stub: unexpected path: $path" >&2; exit 2; }
[[ -n $sha ]] || { echo "gh stub: no head_sha filter" >&2; exit 2; }
jq --arg sha "$sha" '{workflow_runs: [.[] | select(.head_sha == $sha)]}' "$STUB_RUNS"
STUB
chmod +x "$bin/gh"

export PATH="$bin:$PATH" STUB_RUNS="$runs" GITHUB_REPOSITORY=test/repo
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.invalid
export GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.invalid
# Plain error messages: these failures are expected, not CI annotations.
unset GITHUB_ACTIONS GITHUB_OUTPUT NOTES_OUT CI_WORKFLOW

git init --quiet -b main "$repo"
cd "$repo"
git config commit.gpgsign false
git config tag.gpgsign false

# Writes a one-crate workspace whose weft-server is at version $1, with a CHANGELOG
# section for each remaining argument, and commits it.
commit_version() {
	local version=$1 section
	shift
	mkdir -p weft-server/src
	printf '[workspace]\nmembers = ["weft-server"]\nresolver = "2"\n' >Cargo.toml
	printf '[package]\nname = "weft-server"\nversion = "%s"\nedition = "2021"\n' "$version" \
		>weft-server/Cargo.toml
	printf 'fn main() {}\n' >weft-server/src/main.rs
	{
		printf '# Changelog\n\n## [Unreleased]\n\n'
		for section in "$@"; do
			printf '## [%s] - 2026-10-08\n\n### Added\n\n- Things in %s.\n\n' "$section" "$section"
		done
		printf '[Unreleased]: https://example.invalid/compare\n'
	} >CHANGELOG.md
	git add -A
	git commit --quiet -m "weft-server $version"
	git rev-parse HEAD
}

# Records a completed CI run for a commit: ci_run <sha> <conclusion> <created_at>.
ci_run() {
	jq --arg sha "$1" --arg c "$2" --arg t "$3" \
		'. + [{head_sha: $sha, status: "completed", conclusion: $c, created_at: $t,
			html_url: "https://example.invalid/run"}]' "$runs" >"$runs.new"
	mv "$runs.new" "$runs"
}

ci_pending() {
	jq --arg sha "$1" '. + [{head_sha: $sha, status: "in_progress", conclusion: null,
		created_at: "2026-10-08T23:00:00Z", html_url: "https://example.invalid/run"}]' \
		"$runs" >"$runs.new"
	mv "$runs.new" "$runs"
}

echo '[]' >"$runs"

# main: 1.2.3 (tagged, CI green), then 1.3.0 without a CHANGELOG section.
good=$(commit_version 1.2.3 1.2.3)
git tag -a v1.2.3 -m v1.2.3
git tag v1.2.4 # a tag whose name does not match the crate version
ci_run "$good" success 2026-10-08T10:00:00Z
ci_run "$good" cancelled 2026-10-08T11:00:00Z # a later cancelled run is ignored

no_notes=$(commit_version 1.3.0 1.2.3)
git tag v1.3.0
ci_run "$no_notes" success 2026-10-08T10:00:00Z

# A green commit whose CI later went red on a newer run.
red=$(commit_version 2.0.0-rc.2 2.0.0-rc.2)
git tag v2.0.0-rc.2
ci_run "$red" success 2026-10-08T10:00:00Z
ci_run "$red" failure 2026-10-08T12:00:00Z

# CI has only an in-progress run.
pending=$(commit_version 2.0.0-rc.3 2.0.0-rc.3)
git tag v2.0.0-rc.3
ci_pending "$pending"

# A release candidate, at the head of main.
rc=$(commit_version 2.0.0-rc.1 2.0.0-rc.1 1.2.3)
git tag v2.0.0-rc.1
ci_run "$rc" success 2026-10-08T10:00:00Z

git update-ref refs/remotes/origin/main HEAD

# A patch release on release/1.2, branched from v1.2.3.
git checkout --quiet -b release-1.2 "$good"
patch=$(commit_version 1.2.5 1.2.5)
git tag v1.2.5
ci_run "$patch" success 2026-10-08T10:00:00Z
git update-ref refs/remotes/origin/release/1.2 HEAD

# A commit on neither main nor a release branch, otherwise fine.
git checkout --quiet -b side "$good"
stray=$(commit_version 1.4.0 1.4.0)
git tag v1.4.0
ci_run "$stray" success 2026-10-08T10:00:00Z

git checkout --quiet main
git branch --quiet -D release-1.2 side

failures=0
out=$tmp/out

# expect ok <description> <verify.sh args...>
# expect fail:<reason> <description> <verify.sh args...>
#   The run must fail, and its message must contain <reason>, so that each case fails
#   for the reason it tests rather than for an earlier one.
expect() {
	local want=$1 description=$2 got reason=''
	shift 2
	if [[ $want == fail:* ]]; then
		reason=${want#fail:}
		want=fail
	fi
	: >"$out"
	if GITHUB_OUTPUT=$out "$verify" "$@" >"$tmp/log" 2>&1; then got=ok; else got=fail; fi
	if [[ $got == "$want" ]] && { [[ -z $reason ]] || grep -qF -- "$reason" "$tmp/log"; }; then
		printf 'ok    %s\n' "$description"
	else
		printf 'FAIL  %s: wanted %s%s, got %s\n' "$description" "$want" "${reason:+ ($reason)}" "$got"
		sed 's/^/      /' "$tmp/log"
		failures=$((failures + 1))
	fi
}

# expect_output <key> <value>: checks the last run's $GITHUB_OUTPUT.
expect_output() {
	if grep -qxF "$1=$2" "$out"; then
		printf 'ok      output %s=%s\n' "$1" "$2"
	else
		printf 'FAIL    output %s=%s; got:\n' "$1" "$2"
		sed 's/^/      /' "$out"
		failures=$((failures + 1))
	fi
}

echo "release (tag mode)"
expect ok "a good tag" tag v1.2.3
expect_output sha "$good"
expect_output version 1.2.3
expect_output tag v1.2.3
expect_output prerelease false
expect ok "a release candidate is a prerelease" tag v2.0.0-rc.1
expect_output prerelease true
expect ok "a patch release on an origin/release/* branch" tag v1.2.5
expect_output sha "$patch"
expect fail:"does not exist" "a missing tag" tag v9.9.9
expect fail:"does not match" "a tag that does not match the crate version" tag v1.2.4
expect fail:"not an ancestor" "a commit on neither main nor a release branch" tag v1.4.0
expect fail:"concluded failure" "a red CI conclusion on the latest run" tag v2.0.0-rc.2
expect fail:"no completed run" "no completed CI run" tag v2.0.0-rc.3
expect fail:"no non-empty '## [1.3.0]' section" "a missing CHANGELOG section" tag v1.3.0
expect fail:"tag must be" "a tag that is not vX.Y.Z or vX.Y.Z-rc.N" tag v1.2
expect fail:"tag must be" "a tag with a build suffix" tag v1.2.3+build
expect fail:"tag must be" "a tag with a non-rc prerelease" tag v1.2.3-beta.1
expect fail:"tag must be" "an unanchored tag" tag xv1.2.3
expect fail:"tag must be" "a tag with a leading zero" tag v01.2.3

# Shell metacharacters must be refused before anything uses them.
pwned=$tmp/pwned
for evil in "v1.2.3;touch $pwned" "v1.2.3\$(touch $pwned)" "\$(touch $pwned)" \
	"v1.2.3\`touch $pwned\`" "v1.2.3 && touch $pwned" $'v1.2.3\ntouch '"$pwned" "v1.2.3|touch $pwned"; do
	expect fail:"tag must be" "a tag with shell metacharacters: $(printf '%q' "$evil")" tag "$evil"
done
if [[ -e $pwned ]]; then
	echo "FAIL  a metacharacter tag ran a command"
	failures=$((failures + 1))
fi

echo "dry run (ref mode)"
expect ok "main" ref main
expect_output sha "$rc"
expect_output tag v2.0.0-rc.1
expect_output prerelease true
expect ok "a commit on main with no CHANGELOG section, by id" ref "$no_notes"
expect_output version 1.3.0
expect_output tag v1.3.0
expect ok "a release branch" ref release/1.2
expect ok "an abbreviated commit id" ref "${good:0:12}"
expect fail:"not an ancestor" "a commit on neither main nor a release branch" ref "$stray"
expect fail:"no completed run" "a commit with no completed CI run" ref "$pending"
expect fail:"does not resolve" "a ref that does not exist" ref no-such-branch
expect fail:"not a usable ref" "a ref with shell metacharacters" ref "main;touch $pwned"
expect fail:"not a usable ref" "a ref range" ref "main..side"
expect fail:"not a usable ref" "an option-like ref" ref --all

echo "usage"
expect fail:usage "no arguments"
expect fail:usage "an unknown mode" release v1.2.3

if [[ $(git worktree list | wc -l) -ne 1 ]]; then
	echo "FAIL  verify.sh left a worktree behind"
	git worktree list
	failures=$((failures + 1))
fi

if ((failures > 0)); then
	echo "$failures failure(s)"
	exit 1
fi
echo "all passed"
