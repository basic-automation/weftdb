//! `GET /api/v1/version` — which build this is, which store it serves, and what its
//! exit statuses mean (FRE-12b).
//!
//! `/health` answers "is the process up" and `/ready` answers "should traffic come
//! here"; both carry the package version because a deployed instance has to be
//! identifiable from a plain `curl`. Neither answers the three questions an operator
//! actually asks when something is wrong at 3 a.m.:
//!
//! 1. **Which build is this, exactly?** `--version` answers it on the machine, but the
//!    machine is usually behind a proxy and the answer is in a log line that has since
//!    rotated. `debug_assertions` distinguishes a debug build from a release one —
//!    a difference in throughput of several times, so "the staging box is slow" is often
//!    a profile question and nobody had a probe to settle it.
//! 2. **Which store is it serving, and does this build agree with it?** The scope pin
//!    ([`crate::store_open::pin_store_scope`]) refuses a mismatched scope at startup;
//!    this is the same comparison while the server is up, so a deployment that runs with
//!    `WEFT_STORE_SCOPE_PIN=off` is *legible* rather than silently trusting. The layout
//!    numbers answer the other half: whether this build may write the store it has open,
//!    or is serving it report-only because the store is newer.
//! 3. **What did the supervisor's exit status mean?** The plan C7 table, in the body a
//!    monitoring host can read instead of a doc page — the class a crashed container
//!    exits with is only useful if the number can be looked up without shell access.
//!
//! ## Why this needs a key
//!
//! The route is [`Scope::Read`], not [`Scope::Probe`]: it names layout numbers, the
//! store's UUID and the scope it serves, and a store's shape is exactly what an
//! unauthenticated probe should not hand out. The documented exposure rule holds — with
//! auth on, only `/health` and `/ready` answer without a key ([`crate::settings`]'
//! registry and `docs/reference.md` both say so) — and the anonymous operator who needs
//! the version before they have a key still has `/ready`, which carries service, version
//! and store UUID. No path is in the body for the same reason: the startup log line
//! ([`crate::store_open::store_identity_line`]) names the root for whoever can read the
//! process's output.

use serde::Serialize;

use crate::{cli::EXIT_CODES, store_open::ServingScope, AppState, SERVICE, VERSION};

/// The whole `GET /api/v1/version` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionResponse {
	/// Logical service name ([`SERVICE`]), as `/health` and `/ready` echo it.
	pub service: &'static str,
	/// Running build version ([`VERSION`]), the package version.
	pub version: &'static str,
	/// How this binary was built ([`BuildInfo`]).
	pub build: BuildInfo,
	/// The store this process serves, or `{"configured": false}` when no
	/// `WEFT_SEGMENT_STORE_ROOT` is set.
	pub store: StoreIdentity,
	/// The scope pin: what this process serves, what the store records, and
	/// whether the server refuses a mismatch ([`ScopePinInfo`]).
	pub scope_pin: ScopePinInfo,
	/// The plan C7 exit-status table this process exits with, so a supervisor
	/// can read the meaning of the number it just saw.
	pub exit_codes: Vec<ExitCodeInfo>,
}

/// How the running binary was built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
	/// The operating system, e.g. `linux` ([`std::env::consts::OS`]).
	pub os: &'static str,
	/// The CPU architecture, e.g. `x86_64` ([`std::env::consts::ARCH`]).
	pub arch: &'static str,
	/// The platform family, `unix` or `windows` ([`std::env::consts::FAMILY`]).
	pub family: &'static str,
	/// The target's environment qualifier, e.g. `gnu` or `musl` — the difference
	/// between a working `getrandom` and a hanging one on an Alpine base image.
	/// The compiler's own statement about the target it built for, taken from the
	/// `target_env` cfg: `std::env::consts` names no libc constant on the MSRV, and
	/// cargo sets `CARGO_CFG_TARGET_ENV` only for a build script. Empty where the
	/// target carries no qualifier, as macOS and the bare-metal targets do.
	pub env: &'static str,
	/// The full target triple, e.g. `x86_64-unknown-linux-gnu` — os, arch and
	/// qualifier in the one string an operator pastes into a bug report and a base
	/// image names a binary by. Cargo's `TARGET`, which names the unit being built;
	/// a `rustc` invoked without cargo leaves the field empty rather than guessed
	/// from the host.
	pub target: &'static str,
	/// The compiler that produced the binary, e.g. `1.95.0` — the version an
	/// operator quotes in a bug report, and the one `rustup` may have replaced
	/// since the build, which is why it is recorded at compile time rather than
	/// asked of the host at run time. Null when the build could not ask `rustc`.
	pub rustc: Option<&'static str>,
	/// The commit the binary was built from, short SHA. Null when the source tree
	/// had no git at build time, as a vendored or unpacked source build has.
	pub git_sha: Option<&'static str>,
	/// Whether that tree carried an uncommitted change that could have been
	/// compiled — the reason to read `git_sha` as "approximately this commit".
	/// Null when `git_sha` is.
	pub git_dirty: Option<bool>,
	/// Pointer width in bits (32 or 64), which is what a memory-limit
	/// conversation is actually about.
	pub pointer_width: usize,
	/// Whether assertions are compiled in: a debug build, not a release one.
	pub debug_assertions: bool,
	/// The cargo profile the binary was built under — `release`, `debug`, `test`.
	/// Cargo names the profile only to a build script, so it is read from there; a
	/// binary built outside cargo has none, and this falls back to what the
	/// assertions imply rather than claiming nothing.
	pub profile: &'static str,
}

/// The `target_env` cfg as the string an operator reads.
///
/// The qualifiers cargo's target specs name; anything else — the bare-metal and
/// `none` targets, `wasi` — carries no qualifier at all and reports the empty
/// string, which the field documents as a value rather than a gap.
#[must_use]
pub const fn target_env() -> &'static str {
	if cfg!(target_env = "gnu") {
		"gnu"
	} else if cfg!(target_env = "musl") {
		"musl"
	} else if cfg!(target_env = "msvc") {
		"msvc"
	} else if cfg!(target_env = "ohos") {
		"ohos"
	} else {
		""
	}
}

/// The build the running binary came from.
#[must_use]
pub fn build_info() -> BuildInfo {
	BuildInfo { os: std::env::consts::OS, arch: std::env::consts::ARCH, family: std::env::consts::FAMILY, target: option_env!("WEFTDB_BUILD_TARGET").unwrap_or(""), env: target_env(), pointer_width: std::mem::size_of::<usize>() * 8, rustc: option_env!("WEFTDB_BUILD_RUSTC_SEMVER"), git_sha: option_env!("WEFTDB_BUILD_GIT_SHA"), git_dirty: option_env!("WEFTDB_BUILD_GIT_DIRTY").map(|value| value == "true"), debug_assertions: cfg!(debug_assertions), profile: option_env!("WEFTDB_BUILD_PROFILE").unwrap_or(if cfg!(debug_assertions) { "debug" } else { "release" }) }
}

/// A `(database, subject)` scope, the two halves named as the environment names them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopeInfo {
	/// The database half ([`crate::store_open::DATABASE_ENV`]).
	pub database: String,
	/// The subject half ([`crate::store_open::SUBJECT_ENV`]).
	pub subject: String,
}

/// The scope pin as this process sees it (FRE-12b).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopePinInfo {
	/// `enforced` when a mismatched scope refuses startup, `off` when
	/// `WEFT_STORE_SCOPE_PIN` has turned the pin off.
	pub mode: &'static str,
	/// The scope this process serves: null with no store configured, since a
	/// stateless server serves no namespace.
	pub serving: Option<ScopeInfo>,
	/// The scope the store root's own `STORE_FORMAT` marker records (null
	/// with no store configured).
	pub recorded: Option<ScopeInfo>,
	/// Whether the two agree: null with no store, `false` for a deployment
	/// serving a root under a scope the root does not record, which is only
	/// possible with the pin off.
	pub matches_store: Option<bool>,
}

/// The store this process has open, and whether this build agrees with it.
/// Every field but `configured` is null when no store root is configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreIdentity {
	/// Whether `WEFT_SEGMENT_STORE_ROOT` is set and a store is open — i.e.
	/// whether the `/api/v1/storage` endpoints are live rather than `503`.
	pub configured: bool,
	/// The store's identity, the same UUID `/ready` reports (D-S12).
	pub store_uuid: Option<String>,
	/// What the store promises about durability ([`weftdb::SegmentStore::durability_class`]).
	pub durability_class: Option<&'static str>,
	/// The scope the store records, which is the scope the pin compares against.
	pub recorded_scope: Option<ScopeInfo>,
	/// The layout the store is in.
	pub layout_version: Option<u32>,
	/// The oldest layout a WeftDB must know to read this store.
	pub min_read_layout: Option<u32>,
	/// The oldest layout a WeftDB must know to write this store.
	pub min_write_layout: Option<u32>,
	/// The layout of the last WeftDB that wrote this store. A number above
	/// `supported_layout` means a newer server has been here.
	pub last_written_layout: Option<u32>,
	/// The newest layout *this build* knows ([`weftdb::SUPPORTED_LAYOUT`]).
	pub supported_layout: u32,
	/// The layout an in-progress migration targets; null when none is in
	/// progress (which is the normal state of a settled store).
	pub migrating_to: Option<u32>,
	/// The newest registered migration applied, by id; null until the store
	/// has finished being migrated.
	pub applied_through: Option<String>,
	/// Whether this build may write the store: its write floor is within what
	/// this build knows.
	pub writable_by_this_build: Option<bool>,
	/// Whether recovery is report-only because the store's layout is newer
	/// than this build: it serves reads and refuses to change anything a
	/// newer WeftDB might understand.
	pub report_only: Option<bool>,
	/// Whether the store's own state makes this build refuse to serve it
	/// (the same flag that answers `503` on `/ready`, D-S12).
	pub incompatible_store: Option<bool>,
	/// Whether the process that held this store before this one left a clean-shutdown
	/// flag. A store created and never shut down reports `false`, as does one whose
	/// last process died: the flag is what `shutdown` writes, not the absence of
	/// evidence to the contrary.
	pub previous_clean_shutdown: Option<bool>,
}

/// One row of the plan C7 exit-status table ([`crate::cli::EXIT_CODES`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExitCodeInfo {
	/// The process exit status.
	pub code: i32,
	/// The C7 meaning, in the table's own words — the same words `--help` prints.
	pub meaning: &'static str,
}

impl VersionResponse {
	/// The body for the state a request is being answered under.
	#[must_use]
	pub fn of(state: &AppState) -> Self {
		let store = state.store();
		let serving = state.serving_scope();
		Self { service: SERVICE, version: VERSION, build: build_info(), store: StoreIdentity::of(store), scope_pin: ScopePinInfo::of(store, serving), exit_codes: EXIT_CODES.iter().map(|row| ExitCodeInfo { code: row.code, meaning: row.meaning }).collect() }
	}
}

impl StoreIdentity {
	/// The store section for `store`, or the `configured: false` shape.
	#[must_use]
	pub fn of(store: Option<&std::sync::Arc<weftdb::SegmentStore>>) -> Self {
		let Some(store) = store else {
			return Self { configured: false, store_uuid: None, durability_class: None, recorded_scope: None, layout_version: None, min_read_layout: None, min_write_layout: None, last_written_layout: None, supported_layout: weftdb::SUPPORTED_LAYOUT, migrating_to: None, applied_through: None, writable_by_this_build: None, report_only: None, incompatible_store: None, previous_clean_shutdown: None };
		};
		let format = store.store_format();
		let report = store.recovery_report();
		Self { configured: true, store_uuid: Some(store.store_uuid().to_string()), durability_class: Some(weftdb::SegmentStore::durability_class()), recorded_scope: Some(ScopeInfo { database: format.scope.database.clone(), subject: format.scope.subject.clone() }), layout_version: Some(format.layout_version), min_read_layout: Some(format.min_read_layout), min_write_layout: Some(format.min_write_layout), last_written_layout: Some(format.last_written_layout), supported_layout: weftdb::SUPPORTED_LAYOUT, migrating_to: format.migrating_to, applied_through: format.applied_through.clone(), writable_by_this_build: Some(format.min_write_layout <= weftdb::SUPPORTED_LAYOUT), report_only: Some(store.open_report().recovery_report_only), incompatible_store: Some(report.incompatible_store), previous_clean_shutdown: Some(store.previous_clean_shutdown()) }
	}
}

impl ScopePinInfo {
	/// The pin section for the store the process has open and the scope it
	/// serves.
	#[must_use]
	pub fn of(store: Option<&std::sync::Arc<weftdb::SegmentStore>>, serving: Option<&ServingScope>) -> Self {
		let mode = serving.map_or("enforced", |scope| scope.scope_pin.label());
		let serving = serving.map(|scope| ScopeInfo { database: scope.database.clone(), subject: scope.subject.clone() });
		let recorded = store.map(|store| {
			let format = store.store_format();
			ScopeInfo { database: format.scope.database, subject: format.scope.subject }
		});
		let matches_store = match (serving.as_ref(), recorded.as_ref()) {
			(Some(serving), Some(recorded)) => Some(serving.database == recorded.database && serving.subject == recorded.subject),
			_ => None,
		};
		Self { mode, serving, recorded, matches_store }
	}
}

/// `GET /api/v1/version`: the build, the store, and the exit-code table.
///
/// Answered from process state only — no store read, no control-plane query — so it
/// cannot fail the way a storage read fails, and it answers while a store is being
/// refused (`incompatible_store: true`) rather than after.
pub async fn version(axum::extract::State(state): axum::extract::State<AppState>) -> axum::Json<VersionResponse> {
	axum::Json(VersionResponse::of(&state))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::store_open::ScopePin;

	#[test]
	fn the_build_info_names_the_profile_that_changes_every_number() {
		let build = build_info();
		assert!(!build.os.is_empty() && !build.arch.is_empty());
		assert_eq!(build.pointer_width, usize::BITS as usize, "the width a memory limit is quoted in");
		// Cargo names the profile to the build script, so a test binary reports
		// `test` rather than the assertion flag's guess; the flag still says debug.
		assert!(build.debug_assertions, "a test binary is not a release build");
		assert!(build.profile == "test" || build.profile == "debug", "the profile cargo recorded");
		assert!(!build.target.is_empty() || build.rustc.is_none(), "a cargo build names its target triple");
		assert!(build.git_sha.is_some(), "a build from a checkout names its commit");
		assert_eq!(build.git_dirty.is_some(), build.git_sha.is_some(), "dirty is known exactly when the commit is");
	}

	#[test]
	fn the_exit_codes_are_the_ones_the_process_exits_with() {
		let body = VersionResponse::of(&AppState::new());
		let codes: Vec<i32> = body.exit_codes.iter().map(|row| row.code).collect();
		assert_eq!(codes, vec![0, 1, 2, 65, 70, 75, 78], "the body carries plan C7's whole table, in order");
		let incompatible = body.exit_codes.iter().find(|row| row.code == crate::settings::STORE_EXIT_CODE).expect("65 is in the table");
		assert!(incompatible.meaning.contains("operator action"), "and says what it means: {}", incompatible.meaning);
	}

	#[test]
	fn a_stateless_server_says_it_has_no_store() {
		let body = VersionResponse::of(&AppState::new());
		assert_eq!((body.service, body.version), (SERVICE, VERSION));
		assert!(!body.store.configured);
		assert_eq!(body.store.supported_layout, weftdb::SUPPORTED_LAYOUT, "what this build knows is known either way");
		assert_eq!(body.scope_pin.serving, None);
		assert_eq!(body.scope_pin.matches_store, None, "nothing is pinned and nothing disagrees");
		let json = serde_json::to_value(&body).expect("serialises");
		assert_eq!(json["store"]["configured"], serde_json::json!(false));
		assert!(json["store"]["store_uuid"].is_null());
	}

	#[tokio::test]
	async fn a_store_reports_its_layout_and_the_scope_the_pin_reads() {
		let dir = tempfile::tempdir().expect("tempdir");
		let store = std::sync::Arc::new(weftdb::SegmentStore::open_scoped(dir.path().join("store"), "market", "BTCUSD").await.expect("opens a store"));
		let state = AppState::new().with_store(store).with_serving_scope(ServingScope::new("market", "BTCUSD", ScopePin::Enforced));
		let body = VersionResponse::of(&state);
		drop(state);
		assert!(body.store.configured);
		assert!(body.store.store_uuid.as_deref().is_some_and(|uuid| uuid.len() == 36), "the same identity /ready reports");
		assert_eq!(body.store.layout_version, Some(weftdb::SUPPORTED_LAYOUT), "a new store is in this build's layout");
		assert_eq!(body.store.writable_by_this_build, Some(true));
		assert_eq!(body.store.report_only, Some(false));
		assert_eq!(body.store.incompatible_store, Some(false));
		assert_eq!(body.scope_pin.mode, "enforced");
		assert_eq!(body.scope_pin.serving.as_ref().map(|s| (s.database.as_str(), s.subject.as_str())), Some(("market", "BTCUSD")));
		assert_eq!(body.scope_pin.matches_store, Some(true), "the scope the process serves is the one the store records");
	}

	#[tokio::test]
	async fn a_pin_turned_off_is_legible_in_the_body() {
		let dir = tempfile::tempdir().expect("tempdir");
		// The store was created under one scope; the process serves another, which
		// is only possible with WEFT_STORE_SCOPE_PIN=off.
		let root = dir.path().join("store");
		let store = std::sync::Arc::new(weftdb::SegmentStore::open_scoped(&root, "market", "BTCUSD").await.expect("opens a store"));
		store.shutdown().await.expect("shuts down");
		drop(store);
		let store = std::sync::Arc::new(weftdb::SegmentStore::open_scoped(&root, "default", "default").await.expect("reopens under another scope"));
		let body = VersionResponse::of(&AppState::new().with_store(store).with_serving_scope(ServingScope::new("default", "default", ScopePin::Off)));
		assert_eq!(body.scope_pin.mode, "off", "a deployment running unpinned says so");
		assert_eq!(body.scope_pin.recorded.as_ref().map(|s| (s.database.as_str(), s.subject.as_str())), Some(("market", "BTCUSD")));
		assert_eq!(body.scope_pin.matches_store, Some(false), "and the mismatch it is accepting is on the record");
	}
}
