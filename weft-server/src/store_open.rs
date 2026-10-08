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

use std::sync::Arc;

use tokio::task::JoinHandle;
use weftdb::{Poisoned, SegmentStore, SegmentStoreOptions};
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

/// Exit the process with [`AMBIGUOUS_COMMIT_EXIT_CODE`] as soon as `store` is
/// write-poisoned, saying why: what `WEFT_ON_AMBIGUOUS_COMMIT=exit` asks for. The task
/// runs for the life of the process.
#[must_use]
pub fn spawn_exit_on_poison(store: Arc<SegmentStore>) -> JoinHandle<()> {
	tokio::spawn(async move {
		let poisoned = store.wait_until_poisoned().await;
		exit_poisoned(&store, &poisoned);
	})
}

/// Log why the store is poisoned and exit with [`AMBIGUOUS_COMMIT_EXIT_CODE`].
fn exit_poisoned(store: &SegmentStore, poisoned: &Poisoned) -> ! {
	let root = store.root().display();
	tracing::error!(%root, reason = %poisoned.reason, "the segment store is write-poisoned; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} as {AMBIGUOUS_COMMIT_ENV}=exit asks, so that the restart's recovery settles it");
	eprintln!("weft-server: segment store {root} is write-poisoned by {}; exiting with status {AMBIGUOUS_COMMIT_EXIT_CODE} ({AMBIGUOUS_COMMIT_ENV}=exit)", poisoned.reason);
	std::process::exit(AMBIGUOUS_COMMIT_EXIT_CODE)
}

#[cfg(test)]
mod tests {
	use super::*;

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
