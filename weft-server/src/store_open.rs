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
//!
//! [`open_from_env`] makes both decisions and opens the store: what the server's startup
//! calls.

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
/// neither [`DATABASE_ENV`] nor [`SUBJECT_ENV`] is set. Spelled out here rather than
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

/// Open the segment store at `root` as `weft-server` does.
///
/// It opens with the options the environment describes ([`store_options_from_env`]),
/// under the scope the environment describes ([`store_scope_from_env`]), and, when
/// `WEFT_ON_AMBIGUOUS_COMMIT=exit` ([`OnPoison::from_env`]), starts the task that exits
/// the process once the store is write-poisoned ([`spawn_exit_on_poison`]).
///
/// # Errors
///
/// The store's open failing ([`SegmentStore::open_scoped_with_options`]).
pub async fn open_from_env(root: impl AsRef<Path>) -> anyhow::Result<SegmentStore> {
	let (database, subject) = store_scope_from_env();
	open_scoped_with_env_options(root, &database, &subject).await
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
	fn the_poison_choice_reads_poison_or_exit() {
		assert_eq!(OnPoison::parse(None), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some("")), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some("poison")), OnPoison::Serve);
		assert_eq!(OnPoison::parse(Some(" EXIT ")), OnPoison::Exit);
		assert_eq!(OnPoison::parse(Some("exit")), OnPoison::Exit);
		assert_eq!(OnPoison::parse(Some("abort")), OnPoison::Serve, "an unknown value keeps the safe default");
	}
}
