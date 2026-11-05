//! How `weft-server` opens its segment store: with the options its environment describes,
//! and with what it does once the store is write-poisoned (release plan C-1).
//!
//! The `weftdb` library reads no `WEFT_*` variable on its own and never exits the
//! process: a store opened with [`SegmentStore::open`] runs with
//! [`SegmentStoreOptions::default`], and an ambiguous COMMIT only poisons it. The server
//! owns both decisions instead:
//!
//! - [`store_options_from_env`] maps `WEFT_SEGMENT_CHECKPOINT_*`,
//!   `WEFT_SEGMENT_PARTIAL_*` and `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` to the options
//!   it opens the store with;
//! - [`OnPoison::from_env`] reads `WEFT_ON_AMBIGUOUS_COMMIT`. Under `exit`,
//!   [`spawn_exit_on_poison`] waits for the store's poison (its own ambiguous COMMIT, or
//!   the process-wide poison) and exits with status 70, so a supervisor restarts the
//!   server and the restart's recovery settles the commit.
//! - [`pin_store_scope`] compares the scope the environment names against the scope the
//!   store root itself records, and refuses the start (exit 65, plan C7's "store needs
//!   operator action") when they differ (FRE-12b).
//!
//! [`open_from_env`] makes all three decisions and opens the store: what the server's
//! startup calls.
//!
//! ## Why the pin exists (FRE-12b)
//!
//! A Storage-v2 root holds several `(database, subject)` scopes, and the scope the server
//! serves is two environment variables. Get one of them wrong — a renamed database, a
//! unit file copied to another deployment, a `WEFT_SEGMENT_STORE_ROOT` that points at the
//! neighbour's volume — and nothing is *wrong* as far as the open is concerned: it finds a
//! valid store, opens a scope that happens to be empty, and serves `404 aspect_unknown`
//! for data the operator believes is right there. That reads as data loss, and it gets
//! worse, because the server then starts writing new aspects into the phantom scope.
//!
//! So the server pins the scope its root records. The store lane is deliberately *not*
//! pinned: one root holding several scopes is a designed feature (the legacy seal opens
//! the same root under `(database, "legacy")`, and the store's own tests bless it), so a
//! refusal belongs at the one place with a single scope to serve — this process. It runs
//! before the open, so a refusal writes nothing; a root with no marker yet has no scope to
//! disagree with, and takes the scope of the open that creates it. `WEFT_STORE_SCOPE_PIN`
//! turns it off for a deployment that really does serve one root under several scopes
//! ([`ScopePin`]), and [`crate::import_rows_mode`] is not pinned at all: it writes the
//! scope the migrated tree names, which is never the environment's.

use std::path::Path;

use tokio::{sync::watch, task::JoinHandle};
use weftdb::{durable::poison_global, Poisoned, SegmentStore, SegmentStoreOptions};
pub use weftdb::{AMBIGUOUS_COMMIT_ENV, AMBIGUOUS_COMMIT_EXIT_CODE};

/// What the server does once its store is write-poisoned; see [`AMBIGUOUS_COMMIT_ENV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnPoison {
	/// Keep serving: writes answer `503`, reads go on, and `/ready` reports
	/// `restart_required`. The default.
	Serve,
	/// Log and exit the process with [`AMBIGUOUS_COMMIT_EXIT_CODE`].
	Exit,
}

impl OnPoison {
	/// Read the choice from [`AMBIGUOUS_COMMIT_ENV`].
	#[must_use]
	pub fn from_env() -> Self {
		Self::parse(std::env::var(AMBIGUOUS_COMMIT_ENV).ok().as_deref())
	}

	/// The choice `value` names: `exit` (any case, trimmed) exits, unset, empty or
	/// `poison` serves. An unknown value keeps serving, the safe default that keeps reads
	/// up, and says so.
	#[must_use]
	pub fn parse(value: Option<&str>) -> Self {
		match value.map(str::trim) {
			Some(exit) if exit.eq_ignore_ascii_case("exit") => Self::Exit,
			None | Some("") => Self::Serve,
			Some(poison) if poison.eq_ignore_ascii_case("poison") => Self::Serve,
			Some(other) => {
				tracing::warn!(value = other, "{AMBIGUOUS_COMMIT_ENV} is neither `poison` nor `exit`; an ambiguous COMMIT will write-poison the store");
				Self::Serve
			}
		}
	}
}

/// The options the server opens its store with: what the environment describes
/// ([`SegmentStoreOptions::from_env`]).
#[must_use]
pub fn store_options_from_env() -> SegmentStoreOptions {
	SegmentStoreOptions::from_env()
}

/// The environment variable naming the `(database, subject)` scope the store is opened
/// under — the half of a store's address that says which tenant's namespace its declared
/// aspects and rows live in.
///
/// A Storage v2 root is a container of scopes, not one scope: the same root can hold
/// `market/BTCUSD` and `iot/sensor-7`, and which one a process sees is decided by the
/// scope its open asks for. Before this variable existed the server always asked for the
/// default scope, so a directory written under any other scope — a migrated 0.1.x
/// database above all — read as empty.
pub const DATABASE_ENV: &str = "WEFT_STORE_DATABASE";

/// The subject half of the scope; see [`DATABASE_ENV`].
pub const SUBJECT_ENV: &str = "WEFT_STORE_SUBJECT";

/// The scope `weftdb::SegmentStore::open` uses, and the scope a server runs under when
/// neither [`DATABASE_ENV`] nor [`SUBJECT_ENV`] is set.
///
/// Spelled out here rather than
/// imported because `weftdb` keeps its copy private, and a server that drifted from it
/// would silently open a different store than `cargo run -p weftdb` does.
pub const DEFAULT_SCOPE_DATABASE: &str = "default";

/// The subject half of [`DEFAULT_SCOPE_DATABASE`].
pub const DEFAULT_SCOPE_SUBJECT: &str = "default";

/// The scope to open a store under, from the environment.
///
/// A variable that is set wins as it stands (a scope name is data, and `weftdb` validates
/// it at the catalog boundary rather than here); a variable that is unset or empty takes
/// the default, so an operator who sets only one half still gets the other half's default
/// rather than an empty name.
#[must_use]
pub fn store_scope_from_env() -> (String, String) {
	(scope_half(std::env::var(DATABASE_ENV).ok().as_deref(), DEFAULT_SCOPE_DATABASE), scope_half(std::env::var(SUBJECT_ENV).ok().as_deref(), DEFAULT_SCOPE_SUBJECT))
}

/// One half of a scope: `value` when it holds something, `default` when it is unset or
/// blank. Kept out of [`store_scope_from_env`] so the rule is testable without editing a
/// process environment that other tests are reading.
fn scope_half(value: Option<&str>, default: &str) -> String {
	value.map(str::trim).filter(|name| !name.is_empty()).map_or_else(|| default.to_string(), ToString::to_string)
}

/// The scope this process serves, and the pin mode it was opened under (FRE-12b).
///
/// [`store_scope_from_env`] resolves the scope and [`pin_store_scope`] checks it, both
/// during startup; this is the pair kept on the router state so `GET /api/v1/version`
/// can report what the process is serving beside what the store records. A server with no
/// store root serves no namespace at all and so carries none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingScope {
	/// The database half, as the environment named it (or the default).
	pub database: String,
	/// The subject half, as the environment named it (or the default).
	pub subject: String,
	/// Whether a mismatch against the store's own scope refused this start, or
	/// `WEFT_STORE_SCOPE_PIN=off` let it through.
	pub scope_pin: ScopePin,
}

impl ServingScope {
	/// The scope a start resolved to, under pin mode `scope_pin`.
	#[must_use]
	pub fn new(database: impl Into<String>, subject: impl Into<String>, scope_pin: ScopePin) -> Self {
		Self { database: database.into(), subject: subject.into(), scope_pin }
	}
}

/// The variable that turns the store-scope pin off (FRE-12b).
pub const SCOPE_PIN_ENV: &str = "WEFT_STORE_SCOPE_PIN";

/// Whether the server pins the scope its store root records (FRE-12b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopePin {
	/// Refuse a start whose scope differs from the one the root records. The default:
	/// serving the wrong scope looks like an empty database, so it needs an explicit
	/// decision rather than a typo.
	#[default]
	Enforced,
	/// Serve whatever scope the environment names, as before FRE-12b.
	Off,
}

impl ScopePin {
	/// The mode `WEFT_STORE_SCOPE_PIN` describes; unset or unreadable is
	/// [`ScopePin::Enforced`], the safe default. An unaccepted value is refused earlier,
	/// at strict configuration ([`crate::settings::value_errors_for`]), so the fallback
	/// here is defence in depth for a process that skips that phase.
	#[must_use]
	pub fn from_env() -> Self {
		Self::parse(std::env::var(SCOPE_PIN_ENV).ok().as_deref())
	}

	/// The mode one environment value describes. Anything that names neither side
	/// (including the blank a shell leaves behind) is [`ScopePin::Enforced`].
	#[must_use]
	pub fn parse(value: Option<&str>) -> Self {
		let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
			return Self::Enforced;
		};
		if SCOPE_PIN_FALSEY.iter().any(|off| value.eq_ignore_ascii_case(off)) {
			Self::Off
		} else {
			Self::Enforced
		}
	}

	/// `Some` for a value the variable accepts, `None` for one it does not: the same
	/// classifier [`ScopePin::parse`] falls back from, shared so strict configuration and
	/// the reader cannot disagree about what `off` means.
	#[must_use]
	pub fn parse_strict(value: &str) -> Option<Self> {
		let value = value.trim();
		if value.is_empty() {
			return Some(Self::default());
		}
		if SCOPE_PIN_FALSEY.iter().any(|off| value.eq_ignore_ascii_case(off)) {
			return Some(Self::Off);
		}
		SCOPE_PIN_TRUTHY.iter().any(|on| value.eq_ignore_ascii_case(on)).then_some(Self::Enforced)
	}

	/// The mode's name as it reads in `GET /api/v1/version`.
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::Enforced => "enforced",
			Self::Off => "off",
		}
	}

	/// Whether a mismatched scope is refused.
	#[must_use]
	pub const fn enforced(self) -> bool {
		matches!(self, Self::Enforced)
	}
}

/// The values that turn the pin off, mirroring the workspace's truthy convention.
const SCOPE_PIN_FALSEY: &[&str] = &["off", "0", "false", "no"];

/// The values that keep the pin on, for a written-down default.
const SCOPE_PIN_TRUTHY: &[&str] = &["on", "1", "true", "yes"];

/// `WEFT_STORE_SCOPE_PIN`'s accepted values, for its registry entry and the strict
/// configuration check to quote.
pub const SCOPE_PIN_VALUES: &str = "`on`/`1`/`true`/`yes` keep the pin on and `off`/`0`/`false`/`no` turn it off (any case)";

/// A store root that records a scope other than the one this server is configured to
/// serve (FRE-12b).
///
/// The refusal startup exits 65 with, because the operator has to fix either the environment
/// or the root before the server can serve the data it is pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeMismatch {
	/// The root that refused.
	pub root: std::path::PathBuf,
	/// The scope the root's `STORE_FORMAT` marker records.
	pub recorded: (String, String),
	/// The scope the environment names.
	pub requested: (String, String),
}

impl std::fmt::Display for ScopeMismatch {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let (recorded_db, recorded_subject) = &self.recorded;
		let (requested_db, requested_subject) = &self.requested;
		write!(f, "store root {} records the scope {recorded_db}/{recorded_subject}, but this server is configured for scope {requested_db}/{requested_subject} ({DATABASE_ENV}, {SUBJECT_ENV}): it would serve a namespace of that store that is empty, not the data the store holds. Set both variables to the store's own scope, point {STORE_ROOT_ENV} at the root you mean, or set {SCOPE_PIN_ENV}=off to serve this root under this scope anyway", self.root.display())
	}
}

impl std::error::Error for ScopeMismatch {}

/// The scope a store root records, or `None` when the root holds no `STORE_FORMAT` marker
/// yet — a root that has never been opened by this build has no scope to disagree with
/// (FRE-12b).
///
/// # Errors
///
/// A marker that exists but is not a marker ([`weftdb::StoreError::UnreadableStoreFormat`]),
/// or a root that cannot be read.
pub async fn recorded_scope(root: &Path) -> anyhow::Result<Option<(String, String)>> {
	match weftdb::store_format::read_marker(root).await {
		Ok(Some(format)) => Ok(Some((format.scope.database, format.scope.subject))),
		Ok(None) => Ok(None),
		Err(error) => Err(error.into()),
	}
}

/// Refuse a scope the store root disagrees with ([`ScopePin`], FRE-12b).
///
/// Called by [`open_from_env`] before the open, so a refusal leaves the root exactly as it
/// found it. It is a comparison against the marker's recorded scope, which is what the
/// store itself says it is: an unmatched marker (a root whose marker was lost) is rebuilt
/// by the open from `store_meta`, which mirrors no scope, so the first open of such a root
/// records the scope it came with and every pin afterwards measures against that.
///
/// # Errors
///
/// [`ScopeMismatch`] when the pin is on and the root records another scope, and the
/// marker's own failures (see [`recorded_scope`]).
pub async fn pin_store_scope(root: &Path, database: &str, subject: &str, pin: ScopePin) -> anyhow::Result<()> {
	if !pin.enforced() {
		return Ok(());
	}
	let Some((recorded_db, recorded_subject)) = recorded_scope(root).await? else {
		return Ok(());
	};
	if recorded_db != database || recorded_subject != subject {
		return Err(ScopeMismatch { root: root.to_path_buf(), recorded: (recorded_db, recorded_subject), requested: (database.to_string(), subject.to_string()) }.into());
	}
	Ok(())
}

/// The environment variable naming the segment-store root directory, read by startup
/// phase 3 ([`crate::state`]) and by the `import-rows-mode` command.
pub const STORE_ROOT_ENV: &str = "WEFT_SEGMENT_STORE_ROOT";

/// The store root the environment names.
///
/// # Errors
///
/// [`STORE_ROOT_ENV`] being unset or empty: the caller then says which command needed a
/// store and how to give it one.
pub fn store_root_from_env() -> anyhow::Result<std::path::PathBuf> {
	let root = std::env::var(STORE_ROOT_ENV).ok().filter(|value| !value.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("{STORE_ROOT_ENV} is not set: name the store directory to write into, or pass --store"))?;
	Ok(std::path::PathBuf::from(root))
}

/// Why the store filesystem check (PAC-5b) refuses a root before the store is
/// opened: the root is not a usable directory on a filesystem this build can be
/// durable on.
///
/// Every variant is a filesystem refusal in plan C7's terms, so the
/// server exits it with [`crate::settings::CONFIG_EXIT_CODE`] (78).
#[derive(Debug)]
pub enum StoreRootProblem {
	/// The root exists and is not a directory.
	NotDirectory(std::path::PathBuf),
	/// The root (or the parent a missing root would be created under) cannot be
	/// created, listed, written or synced.
	Unusable(std::path::PathBuf, std::io::Error),
}

impl std::fmt::Display for StoreRootProblem {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::NotDirectory(root) => write!(f, "store root {} exists and is not a directory", root.display()),
			Self::Unusable(root, error) => write!(f, "store root {} is not usable by this server: {error}", root.display()),
		}
	}
}

impl std::error::Error for StoreRootProblem {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::NotDirectory(_) => None,
			Self::Unusable(_, error) => Some(error),
		}
	}
}

/// The store filesystem check (PAC-5b).
///
/// Refuse `root` before the store opens when the filesystem cannot hold a store — the root
/// exists as a file, cannot be created or listed, is not writable, or (on Unix) its directory
/// entry cannot be synced, which is the durability every store write assumes.
///
/// A missing root is
/// created, as the open itself would create it, and then probed. The probe file
/// is removed before this returns, success or failure.
///
/// # Errors
///
/// [`StoreRootProblem`] naming the root and the failing operation.
pub fn check_store_root(root: &Path) -> Result<(), StoreRootProblem> {
	let unusable = |error: std::io::Error| StoreRootProblem::Unusable(root.to_path_buf(), error);
	match std::fs::metadata(root) {
		Ok(metadata) if !metadata.is_dir() => return Err(StoreRootProblem::NotDirectory(root.to_path_buf())),
		Ok(_) => {}
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(root).map_err(unusable)?,
		Err(error) => return Err(unusable(error)),
	}
	// Listing is how the open reads the root; writing plus a file sync is how it
	// creates the marker and control planes; a directory sync is what makes a
	// rename inside it durable.
	std::fs::read_dir(root).map_err(unusable)?.next().transpose().map_err(unusable)?;
	let probe = root.join(format!(".weft-fs-check-{}", std::process::id()));
	let probe_result = (|| -> std::io::Result<()> {
		{
			let mut file = std::fs::File::create(&probe)?;
			std::io::Write::write_all(&mut file, b"weftdb store filesystem check")?;
			file.sync_all()?;
		}
		sync_directory(root)
	})();
	let _ = std::fs::remove_file(&probe);
	probe_result.map_err(unusable)
}

/// Sync a directory's entry to disk. Unix only: opening a directory as a file is
/// how its fsync is spelled there, and Windows has no equivalent this check can
/// portably perform, so there the write probe above stands on its own.
#[cfg(unix)]
fn sync_directory(dir: &Path) -> std::io::Result<()> {
	std::fs::File::open(dir)?.sync_all()
}

/// See [`sync_directory`].
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) -> std::io::Result<()> {
	Ok(())
}

/// The exit status (plan C7) a store-open failure maps to, when it maps to one:
///
/// [`crate::settings::ROOT_LOCKED_EXIT_CODE`] (75) for a root another process
/// holds, [`crate::settings::STORE_EXIT_CODE`] (65) for a [`weftdb::StoreError`]
/// — the store needs operator action. Anything else keeps the caller's default.
/// A [`ScopeMismatch`] is in the same class: the CLI's own table has always named "scope
/// mismatch" under 65, and FRE-12b is the slice that makes the server produce one.
#[must_use]
pub fn exit_code_for_store_error(error: &anyhow::Error) -> Option<i32> {
	if error.downcast_ref::<weftdb::StoreLocked>().is_some() {
		return Some(crate::settings::ROOT_LOCKED_EXIT_CODE);
	}
	if error.downcast_ref::<ScopeMismatch>().is_some() || error.downcast_ref::<weftdb::StoreError>().is_some() {
		return Some(crate::settings::STORE_EXIT_CODE);
	}
	None
}

/// Open the segment store at `root` as `weft-server` does.
///
/// It opens with the options the environment describes ([`store_options_from_env`]),
/// under the scope the environment describes ([`store_scope_from_env`]), having first
/// pinned that scope against the store root's own record ([`pin_store_scope`], FRE-12b)
/// and, when `WEFT_ON_AMBIGUOUS_COMMIT=exit` ([`OnPoison::from_env`]), started the task
/// that exits the process once the store is write-poisoned ([`spawn_exit_on_poison`]).
///
/// # Errors
///
/// A [`ScopeMismatch`] when the store root records a scope other than the environment's,
/// or the store's open failing ([`SegmentStore::open_scoped_with_options`]).
pub async fn open_from_env(root: impl AsRef<Path>) -> anyhow::Result<SegmentStore> {
	open_and_pin_from_env(root).await.map(|(store, _)| store)
}

/// The scope this process serves, and the pin mode the start runs under ([`ServingScope`],
/// FRE-12b).
///
/// The same read [`open_and_pin_from_env`] pins with, so what `GET /api/v1/version` reports
/// is what the refusal was measured with.
#[must_use]
pub fn serving_scope_from_env() -> ServingScope {
	let (database, subject) = store_scope_from_env();
	ServingScope::new(database, subject, ScopePin::from_env())
}

/// [`open_from_env`], also yielding the [`ServingScope`] it opened under.
///
/// Startup
/// keeps that pair so `GET /api/v1/version` reports the scope and the pin mode of the
/// open that actually happened rather than a second read of the environment.
///
/// # Errors
///
/// As [`open_from_env`].
pub async fn open_and_pin_from_env(root: impl AsRef<Path>) -> anyhow::Result<(SegmentStore, ServingScope)> {
	let scope = serving_scope_from_env();
	pin_store_scope(root.as_ref(), &scope.database, &scope.subject, scope.scope_pin).await?;
	let store = open_scoped_with_env_options(root, &scope.database, &scope.subject).await?;
	Ok((store, scope))
}

/// Open the segment store at `root` under an explicit scope, with the options and the
/// poison policy the environment describes.
///
/// This is [`open_from_env`] with the scope given rather than read: `import-rows-mode`
/// writes under the scope the migrated tree names, which is not the environment's — the
/// process doing the migrating runs in the deployment it migrates *from*, whose
/// [`DATABASE_ENV`] points somewhere else entirely.
///
/// # Errors
///
/// The store's open failing ([`SegmentStore::open_scoped_with_options`]).
pub async fn open_scoped_with_env_options(root: impl AsRef<Path>, database: &str, subject: &str) -> anyhow::Result<SegmentStore> {
	let on_poison = OnPoison::from_env();
	let store = SegmentStore::open_scoped_with_options(root, database, subject, store_options_from_env()).await?;
	if on_poison == OnPoison::Exit {
		// It ends on its own when the store drops; its handle is dropped on purpose.
		drop(spawn_exit_on_poison(&store));
	}
	Ok(store)
}

/// The store identity line startup prints once the store is open (PAC-7):
///
/// one line naming the store the process is actually serving — its root,
/// its `store_uuid`, the `(database, subject)` scope it was opened under,
/// its layout as opened, whether the previous shutdown was clean, and
/// what this open did (created the store, opened it, migrated it, or
/// opened a newer layout report-only).
///
/// Identity is reported here, in one formatter every open path can share,
/// because a supervisor's first question after a restart — *which store
/// came up?* — is answered from the startup log or not at all: `/ready`
/// carries the same UUID, so the two can be checked against each other,
/// and the scope names the namespace the process sees, which two roots
/// (or one root under two scopes) otherwise make ambiguous.
#[must_use]
pub fn store_identity_line(store: &SegmentStore) -> String {
	let report = store.open_report();
	let format = store.store_format();
	let how = if report.created_new { "created".to_string() } else { report.migrated_from.map_or_else(|| "opened".to_string(), |layout| format!("migrated from layout {layout}")) };
	let clean = if store.previous_clean_shutdown() { "previous shutdown clean" } else { "previous shutdown not clean" };
	format!("segment store: {how} at {} (store {}, scope {}/{}, layout {}, {clean}; storage endpoints live)", store.root().display(), store.store_uuid(), format.scope.database, format.scope.subject, format.layout_version)
}

/// Exit the process with [`AMBIGUOUS_COMMIT_EXIT_CODE`] as soon as `store` is
/// write-poisoned, by its own ambiguous COMMIT or by the process-wide poison, saying why:
/// what `WEFT_ON_AMBIGUOUS_COMMIT=exit` asks for.
///
/// The task holds the two poison watches and the root's path, not the store, so the
/// store (its databases and `LOCK`) can still drop before the process exits; the task
/// then ends without exiting.
#[must_use]
pub fn spawn_exit_on_poison(store: &SegmentStore) -> JoinHandle<()> {
	let root = store.root().to_path_buf();
	let own = store.subscribe_poison();
	let process = poison_global().subscribe();
	tokio::spawn(async move {
		if let Some(poisoned) = first_poison(own, process).await {
			exit_poisoned(&root, &poisoned);
		}
	})
}

/// The first poison a store's own watch (`own`) or the process-wide one (`process`)
/// reports, as the store's [`SegmentStore::poisoned`] reports it; `None` once the store
/// has dropped (its watch closes) unpoisoned.
async fn first_poison(mut own: watch::Receiver<Option<Poisoned>>, mut process: watch::Receiver<Option<String>>) -> Option<Poisoned> {
	loop {
		// Each borrow is released before anything else happens.
		let (store_poison, process_poison) = (own.borrow_and_update().clone(), process.borrow_and_update().clone());
		if let Some(poisoned) = store_poison {
			return Some(poisoned);
		}
		if let Some(reason) = process_poison {
			return Some(Poisoned { reason: format!("the process is write-poisoned: {reason}") });
		}
		tokio::select! {
			changed = own.changed() => {
				if changed.is_err() {
					return None;
				}
			}
			// The process's sender is a static, never dropped, so this wait cannot fail.
			_ = process.changed() => {}
		}
	}
}

/// Log why the store at `root` is poisoned and exit with [`AMBIGUOUS_COMMIT_EXIT_CODE`].
fn exit_poisoned(root: &Path, poisoned: &Poisoned) -> ! {
	let root = root.display();
	tracing::error!(%root, reason = %poisoned.reason, "the segment store is write-poisoned; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} as {AMBIGUOUS_COMMIT_ENV}=exit asks, so that the restart's recovery settles it");
	eprintln!("weft-server: segment store {root} is write-poisoned by {}; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} ({AMBIGUOUS_COMMIT_ENV}=exit)", poisoned.reason);
	std::process::exit(AMBIGUOUS_COMMIT_EXIT_CODE)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_unset_or_blank_scope_half_takes_the_default() {
		assert_eq!(scope_half(None, "default"), "default");
		assert_eq!(scope_half(Some(""), "default"), "default");
		assert_eq!(scope_half(Some("   "), "default"), "default", "a blank name would open a store under an empty scope rather than the default one");
	}

	#[test]
	fn a_set_scope_half_is_taken_as_it_stands() {
		assert_eq!(scope_half(Some("v010_legacy"), "default"), "v010_legacy");
		assert_eq!(scope_half(Some(" plant "), "default"), "plant", "surrounding space is the shell's, not the name's");
	}

	#[test]
	fn the_default_scope_is_the_one_weftdb_opens() {
		// A server with no scope set must land in the same namespace as `SegmentStore::open`,
		// or every store written before this variable existed reads as empty.
		assert_eq!((DEFAULT_SCOPE_DATABASE, DEFAULT_SCOPE_SUBJECT), ("default", "default"));
	}

	#[test]
	fn the_filesystem_check_accepts_a_usable_root_and_creates_a_missing_one() {
		let dir = tempfile::tempdir().expect("tempdir");
		let root = dir.path().join("store");
		check_store_root(&root).expect("a missing root under a usable parent checks out");
		assert!(root.is_dir(), "the check creates the root, as the open would");
		check_store_root(&root).expect("an existing empty root checks out");
		assert_eq!(std::fs::read_dir(&root).expect("lists").count(), 0, "the probe file is removed");
	}

	#[test]
	fn the_filesystem_check_refuses_a_root_that_is_a_file() {
		let dir = tempfile::tempdir().expect("tempdir");
		let file = dir.path().join("not-a-store");
		std::fs::write(&file, b"data").expect("write");
		assert!(matches!(check_store_root(&file), Err(StoreRootProblem::NotDirectory(_))), "a file cannot hold a store");
		let under_file = file.join("store");
		assert!(check_store_root(&under_file).is_err(), "nor can a root be created under a file");
	}

	#[tokio::test]
	async fn the_identity_line_names_the_store_that_opened() {
		let dir = tempfile::tempdir().expect("tempdir");
		let root = dir.path().join("store");
		let store = SegmentStore::open(&root).await.expect("opens a store");
		let line = store_identity_line(&store);
		assert!(line.contains(&root.display().to_string()), "the root: {line}");
		assert!(line.contains(store.store_uuid()), "the store_uuid: {line}");
		assert!(line.contains("scope default/default"), "the scope it opened under: {line}");
		assert!(line.contains("created"), "a fresh store says it was created: {line}");
		assert!(line.contains("previous shutdown not clean"), "a fresh store was never shut down cleanly: {line}");
		let uuid = store.store_uuid().to_string();
		store.shutdown().await.expect("shuts down cleanly");
		drop(store);
		let reopened = SegmentStore::open(&root).await.expect("reopens");
		let line = store_identity_line(&reopened);
		assert!(line.contains(&uuid), "the identity survives the reopen: {line}");
		assert!(line.contains("opened") && line.contains("previous shutdown clean"), "a reopened store says so: {line}");
		drop(reopened);
	}

	#[test]
	fn the_scope_pin_is_on_unless_the_environment_turns_it_off() {
		assert_eq!(ScopePin::default(), ScopePin::Enforced, "a typo'd scope must not serve an empty namespace quietly");
		for off in ["off", "0", "false", "NO", " off "] {
			assert_eq!(ScopePin::parse(Some(off)), ScopePin::Off, "{off:?} turns the pin off");
		}
		for on in ["on", "1", "true", "YES", ""] {
			assert_eq!(ScopePin::parse(Some(on)), ScopePin::Enforced, "{on:?} keeps the pin on");
		}
		assert_eq!(ScopePin::parse(None), ScopePin::Enforced);
		assert_eq!(ScopePin::parse(Some("maybe")), ScopePin::Enforced, "an unknown value keeps the safe default");
		// The strict classifier agrees with the reader about every value either side accepts,
		// and refuses what neither does; strict configuration turns that into exit 78.
		for value in ["off", "0", "false", "no", "on", "1", "true", "yes"] {
			assert_eq!(ScopePin::parse_strict(value), Some(ScopePin::parse(Some(value))), "{value:?} is accepted the same way by both");
		}
		for junk in ["maybe", "disabled", "nope"] {
			assert_eq!(ScopePin::parse_strict(junk), None, "{junk:?} is no value the variable accepts");
		}
	}

	#[tokio::test]
	async fn the_pin_refuses_a_root_that_records_another_scope() {
		let dir = tempfile::tempdir().expect("tempdir");
		let root = dir.path().join("store");
		// A root that holds no marker yet has no scope to disagree with: the open that
		// creates it records the scope it came with.
		assert_eq!(recorded_scope(&root).await.expect("no marker yet"), None);
		pin_store_scope(&root, "market", "BTCUSD", ScopePin::Enforced).await.expect("an unmarked root is not a mismatch");
		let store = SegmentStore::open_scoped_with_options(&root, "market", "BTCUSD", SegmentStoreOptions::default()).await.expect("opens");
		drop(store);
		assert_eq!(recorded_scope(&root).await.expect("marked now"), Some(("market".to_string(), "BTCUSD".to_string())), "the creating open records the scope");
		pin_store_scope(&root, "market", "BTCUSD", ScopePin::Enforced).await.expect("the store's own scope is no refusal");
		let error = pin_store_scope(&root, "default", "default", ScopePin::Enforced).await.expect_err("another scope is");
		let mismatch = error.downcast_ref::<ScopeMismatch>().expect("the pin's own error");
		assert_eq!(mismatch.recorded, ("market".to_string(), "BTCUSD".to_string()));
		assert_eq!(mismatch.requested, ("default".to_string(), "default".to_string()));
		let message = error.to_string();
		for promise in ["records the scope market/BTCUSD", "configured for scope default/default", DATABASE_ENV, SUBJECT_ENV, STORE_ROOT_ENV, "WEFT_STORE_SCOPE_PIN=off"] {
			assert!(message.contains(promise), "the refusal names what to change ({promise:?}): {message}");
		}
		// The escape hatch the message promises works, and the class is the operator one.
		pin_store_scope(&root, "default", "default", ScopePin::Off).await.expect("the pin off serves it anyway");
		let wrapped = anyhow::Error::new(ScopeMismatch { root: root.clone(), recorded: ("a".into(), "b".into()), requested: ("c".into(), "d".into()) });
		assert_eq!(exit_code_for_store_error(&wrapped), Some(crate::settings::STORE_EXIT_CODE), "a scope mismatch is plan C7's 65");
	}

	#[test]
	fn the_poison_choice_reads_poison_or_exit() {
		assert_eq!(OnPoison::parse(None), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some("")), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some("poison")), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some(" EXIT ")), OnPoison::Exit);
		assert_eq!(OnPoison::parse(Some("exit")), OnPoison::Exit);
		assert_eq!(OnPoison::parse(Some("abort")), OnPoison::Serve, "an unknown value keeps the safe default");
	}
}
