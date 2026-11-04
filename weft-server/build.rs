//! Build-time provenance for `GET /api/v1/version` (1.0 plan FRE-12b).
//!
//! The version route answers one operator question: *what exactly is this
//! process*. The two answers that matter in an incident — which target and libc
//! the binary was built for, and which commit it came from — are facts about the
//! compilation, not about the machine the binary later runs on, so they are
//! captured here rather than probed at run time: a released binary is routinely
//! started on a host with no toolchain and no git checkout at all, where a probe
//! would either fail or, worse, name the host's toolchain as though it had built
//! the binary.
//!
//! Each fact is emitted as a `WEFTDB_BUILD_*` variable and read back in
//! `src/version.rs` with `option_env!`; nothing is emitted for a value that could
//! not be read, so a source tree without git leaves the field absent in the
//! response rather than wrong. The prefix is `WEFTDB_BUILD_`, the one
//! `weft-bench/build.rs` already uses for the same reason: `src/settings.rs` is
//! the registry of variables an *operator* may set and its coverage test walks
//! every crate for `WEFT_`-prefixed literals, and none of these are knobs —
//! exporting one in a shell changes nothing a built binary does.
//!
//! `weft-bench/build.rs` records a wider set (opt-level, LTO, panic strategy,
//! LLVM version) because a benchmark claim needs the whole code-generation
//! story. This script keeps the four facts an operator acts on. The logic is
//! duplicated rather than shared because a build script cannot be imported as a
//! crate, and a workspace helper crate to share forty lines is the heavier cost;
//! the two scripts are allowed to diverge, since a bench manifest and a version
//! endpoint answer different questions.

use std::process::Command;

/// The compiler building this crate, as `1.95.0` — the version an operator
/// quotes in a bug report, and the one `rustup` may have replaced since.
///
/// Cargo points `RUSTC` at the exact binary it is invoking; the `rustc` on
/// `PATH` may be a different toolchain.
fn rustc_semver() -> Option<String> {
	let compiler = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
	let output = Command::new(&compiler).arg("--version").arg("--verbose").output().ok()?;
	if !output.status.success() {
		return None;
	}
	let stdout = String::from_utf8_lossy(&output.stdout);
	stdout.lines().map(str::trim).find_map(|line| line.strip_prefix("release: ")).map(str::trim).filter(|value| !value.is_empty() && *value != "unknown").map(str::to_string)
}

/// The commit the source tree sits on and whether it is modified, as
/// `(sha, "true"|"false")`.
///
/// "Modified" means the tracked tree differs from `HEAD`, or an untracked file
/// that could have been compiled sits next to it. Anything else `git status`
/// reports — scratch notes, agent logs, a build directory — cannot change the
/// program, and counting it would call every build from a working tree dirty.
fn git_facts(dir: &str) -> Option<(String, &'static str)> {
	let output = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
	if !output.status.success() {
		return None;
	}
	let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
	if sha.is_empty() {
		return None;
	}
	// `GIT_OPTIONAL_LOCKS=0` keeps `git status` from rewriting the index, which
	// is watched below: a write here would rerun this script on every build.
	let status = Command::new("git").arg("-C").arg(dir).env("GIT_OPTIONAL_LOCKS", "0").args(["status", "--porcelain"]).output().ok()?;
	if !status.status.success() {
		// Unreadable status is treated as modified: the safe answer to "do you
		// know exactly what produced this binary" is no.
		return Some((sha, "true"));
	}
	let dirty = String::from_utf8_lossy(&status.stdout).lines().any(|line| {
		let Some((code, path)) = line.split_once(' ') else { return true };
		if code != "??" {
			return true;
		}
		let path = path.trim();
		path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
	});
	Some((sha, if dirty { "true" } else { "false" }))
}

/// The files whose change can make a recorded commit stale.
///
/// Watching none of them would leave a binary built after a commit still naming
/// its parent. Watched: git's `HEAD`, its index and the branch `HEAD` names (a
/// commit, a checkout, a staging), and every workspace member's sources and
/// manifest (an edit not yet committed). The paths come from git itself, so a
/// linked worktree — whose `.git` is a file, not a directory — watches its own
/// `HEAD` and index, which is how this script behaves in the per-slice worktrees
/// the 1.0 buildout is developed in.
fn watched_paths(manifest_dir: &str) -> Vec<String> {
	let git = |args: &[&str]| Command::new("git").arg("-C").arg(manifest_dir).args(args).output().ok().filter(|output| output.status.success()).map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|value| !value.is_empty());
	let mut paths = Vec::new();
	let mut git_files = vec!["HEAD".to_string(), "index".to_string()];
	git_files.extend(git(&["symbolic-ref", "-q", "HEAD"]));
	for file in git_files {
		paths.extend(git(&["rev-parse", "--path-format=absolute", "--git-path", &file]));
	}
	let workspace = std::path::Path::new(manifest_dir).join("..");
	let manifest = std::fs::read_to_string(workspace.join("Cargo.toml")).unwrap_or_default();
	let members = manifest.split_once("members = [").map_or("", |(_, rest)| rest.split_once(']').map_or("", |(list, _)| list));
	for member in members.split(',').map(str::trim).map(|member| member.trim_matches('"')).filter(|member| !member.is_empty()) {
		for part in ["src", "Cargo.toml"] {
			paths.push(workspace.join(member).join(part).display().to_string());
		}
	}
	// Cargo treats a watched path that does not exist as always changed, which
	// would rebuild this crate on every build.
	paths.retain(|path| std::path::Path::new(path).exists());
	paths
}

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	println!("cargo:rerun-if-changed=../Cargo.lock");
	let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
	for path in watched_paths(&manifest_dir) {
		println!("cargo:rerun-if-changed={path}");
	}
	// The target triple and its libc qualifier: `x86_64-unknown-linux-musl` in a
	// `gnu` host, or the reverse, is a binary that starts and then hangs inside
	// `getrandom`, and the operator reading `/api/v1/version` is the one who has
	// to notice. `TARGET` and `CARGO_CFG_TARGET_ENV` are cargo's own view of the
	// unit being built; the qualifier is empty where the target carries none, as
	// macOS and the bare-metal targets do, and an empty value is simply absent.
	for (key, name) in [("WEFTDB_BUILD_TARGET", "TARGET"), ("WEFTDB_BUILD_TARGET_ENV", "CARGO_CFG_TARGET_ENV"), ("WEFTDB_BUILD_PROFILE", "PROFILE")] {
		if let Ok(value) = std::env::var(name) {
			if !value.is_empty() {
				println!("cargo:rustc-env={key}={value}");
			}
		}
	}
	if let Some(semver) = rustc_semver() {
		println!("cargo:rustc-env=WEFTDB_BUILD_RUSTC_SEMVER={semver}");
	}
	if let Some((sha, dirty)) = git_facts(&manifest_dir) {
		println!("cargo:rustc-env=WEFTDB_BUILD_GIT_SHA={sha}");
		println!("cargo:rustc-env=WEFTDB_BUILD_GIT_DIRTY={dirty}");
	}
}
