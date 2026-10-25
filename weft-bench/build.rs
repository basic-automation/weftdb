//! Build-time runtime provenance for the run manifest (1.0 plan EVI-3).
//!
//! `metadata.runtime` records the toolchain and the revision that produced the measured binary.
//! It is captured here, at compile time, rather than at run time, for two reasons. The binary is
//! the artifact whose numbers get quoted, and by the time it runs the toolchain that built it may
//! be gone — `rustup` keeps one active toolchain, not the history of them, so a `rustc --version`
//! probe at run time reports whichever toolchain the machine happens to default to now. And a
//! released binary is routinely run on machines with no Rust installation at all, where the probe
//! would simply fail.
//!
//! Each fact is emitted as a `WEFTDB_BUILD_*` environment variable and read back with
//! `option_env!`; nothing is emitted for a value that could not be read, so an unexpected toolchain
//! or a source tree without git leaves the field absent in the manifest rather than wrong. Absent
//! is what keeps a quoted number honest: a run with no recorded compiler cannot be graded as
//! reference grade.
//!
//! The names carry this crate's own `WEFTDB_` prefix rather than the `WEFT_` one on purpose.
//! `weft-server/src/settings.rs` is the registry of variables an operator may set, and its coverage
//! test walks every crate for `WEFT_`-prefixed literals so that a typo of a documented knob is
//! never silent. These are not knobs: each exists only inside the compilation of this crate, set by
//! the lines below and read back by `option_env!`, so exporting one in a shell changes nothing a
//! built binary does. Calling them `WEFT_BUILD_*` would mean putting fourteen entries an operator
//! cannot act on into the list that exists to tell an operator what they can.

use std::process::Command;

/// Ask the compiler that is building this crate what it is.
///
/// Cargo points `RUSTC` at the exact binary it is invoking, which is the one whose version belongs
/// in the manifest; the `rustc` on `PATH` may be a different toolchain.
fn rustc_facts() -> Vec<(String, String)> {
	let compiler = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
	let Ok(output) = Command::new(&compiler).arg("--version").arg("--verbose").output() else {
		return Vec::new();
	};
	if !output.status.success() {
		return Vec::new();
	}
	let stdout = String::from_utf8_lossy(&output.stdout);
	let field = |name: &str| stdout.lines().map(str::trim).find_map(|line| line.strip_prefix(&format!("{name}: ")).map(|value| value.trim().to_string())).filter(|value: &String| !value.is_empty() && value != "unknown");
	let mut facts = Vec::new();
	// The first line is `rustc 1.99.0 (123456789 2026-01-01)`; `release:` repeats the version and
	// is the field that survives formatting changes, so it is the semver we record.
	if let Some(release) = field("release") {
		facts.push(("WEFTDB_BUILD_RUSTC_SEMVER".to_string(), release));
	}
	if let Some(hash) = field("commit-hash").map(|h| h.chars().take(12).collect()) {
		facts.push(("WEFTDB_BUILD_RUSTC_COMMIT".to_string(), hash));
	}
	if let Some(host) = field("host") {
		facts.push(("WEFTDB_BUILD_RUSTC_HOST".to_string(), host));
	}
	if let Some(llvm) = field("llvm-version") {
		facts.push(("WEFTDB_BUILD_LLVM_VERSION".to_string(), llvm));
	}
	facts
}

/// The commit the source tree sits on and whether it is modified, as `(sha, "true"|"false")`.
///
/// "Modified" means the tracked tree differs from `HEAD`, or an untracked file could have been
/// compiled: a `.rs` file, or a manifest. Anything else that `git status` reports — scratch notes,
/// agent logs, a build directory — cannot change the measured program, and counting it would
/// disqualify every run taken in a working tree.
fn git_facts(dir: &str) -> Option<(String, &'static str)> {
	let output = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--short=12", "HEAD"]).output().ok()?;
	if !output.status.success() {
		return None;
	}
	let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
	if sha.is_empty() {
		return None;
	}
	Some((sha, if working_tree_is_modified(dir) { "true" } else { "false" }))
}

/// True when the tree carries a change that could have reached the compiled program.
fn working_tree_is_modified(dir: &str) -> bool {
	let Ok(output) = Command::new("git").arg("-C").arg(dir).args(["status", "--porcelain"]).output() else {
		// Unreadable status is treated as modified: the safe answer to "do you know exactly what
		// produced this number" is no.
		return true;
	};
	String::from_utf8_lossy(&output.stdout).lines().any(|line| {
		let Some((status, path)) = line.split_once(' ') else { return false };
		if status != "??" {
			return true;
		}
		let path = path.trim();
		path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
	})
}

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	println!("cargo:rerun-if-changed=../Cargo.lock");
	let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
	for (key, value) in rustc_facts() {
		println!("cargo:rustc-env={key}={value}");
	}
	if let Some((sha, dirty)) = git_facts(&manifest_dir) {
		println!("cargo:rustc-env=WEFTDB_BUILD_GIT_SHA={sha}");
		println!("cargo:rustc-env=WEFTDB_BUILD_GIT_DIRTY={dirty}");
	}
	// The optimization profile is a fact about the measured numbers, not about the machine:
	// `opt-level=0` and `opt-level=3` are different benchmarks wearing the same workload name, and
	// cargo exposes the profile only to the build script. `DEBUG`/`LTO` are lists with spaces in
	// them, so they are joined with a `+` that `cargo:rustc-env` can carry.
	let list = |name: &str| std::env::var(name).map(|value| value.split_whitespace().collect::<Vec<_>>().join("+")).ok().filter(|value: &String| !value.is_empty());
	let scalars = [("WEFTDB_BUILD_TARGET", "TARGET"), ("WEFTDB_BUILD_PROFILE", "PROFILE"), ("WEFTDB_BUILD_OPT_LEVEL", "OPT_LEVEL"), ("WEFTDB_BUILD_DEBUG_ASSERTIONS", "DEBUG_ASSERTIONS"), ("WEFTDB_BUILD_OVERFLOW_CHECKS", "OVERFLOW_CHECKS"), ("WEFTDB_BUILD_PANIC", "PANIC")];
	for (key, name) in scalars {
		if let Ok(value) = std::env::var(name) {
			if !value.is_empty() {
				println!("cargo:rustc-env={key}={value}");
			}
		}
	}
	for (key, name) in [("WEFTDB_BUILD_DEBUG", "DEBUG"), ("WEFTDB_BUILD_LTO", "LTO")] {
		if let Some(value) = list(name) {
			println!("cargo:rustc-env={key}={value}");
		}
	}
}
