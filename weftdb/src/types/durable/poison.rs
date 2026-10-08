//! The process-wide write poison (robustness track ROB-2, D16).
//!
//! A store poisons itself when one of its COMMITs is ambiguous (`SegmentStore`'s own
//! poison). Some failures are not the store's: a panic in WeftDB's own code in the middle
//! of a control-plane write (inside [`control_plane_write`](crate::exec::control_plane_write))
//! leaves Turso's pager in a state nobody can vouch for, and every store of the process
//! shares that engine. For those, the process's panic hook sets this poison, and every
//! write entry point of every store checks it alongside the store's own: writes are
//! refused until the process restarts, reads keep working, and an embedder that exits on
//! poison (`weft-server` under `WEFT_ON_AMBIGUOUS_COMMIT=exit`) hears about it through
//! [`GlobalPoison::subscribe`]. No store opens meanwhile either, since an open writes
//! (its marker, its migrations, `store_meta` and its scope): reopening a root in the same
//! process would write through the same engine.
//!
//! It is set once and never cleared: only a restart, whose recovery settles the
//! interrupted write, takes it away.

use std::sync::{
	atomic::{AtomicBool, Ordering}, LazyLock, OnceLock
};

use tokio::sync::watch;

/// The process-wide write poison; see the module documentation.
#[derive(Debug)]
pub struct GlobalPoison {
	/// Checked on every write, so it is the one thing a write reads.
	poisoned: AtomicBool,
	/// Why, from the first poisoning; later ones keep it.
	reason: OnceLock<String>,
	/// Tells watchers once the poison is set.
	tx: watch::Sender<Option<String>>,
}

static GLOBAL: LazyLock<GlobalPoison> = LazyLock::new(|| GlobalPoison { poisoned: AtomicBool::new(false), reason: OnceLock::new(), tx: watch::Sender::new(None) });

/// The process-wide write poison.
#[must_use]
pub fn poison_global() -> &'static GlobalPoison {
	&GLOBAL
}

impl GlobalPoison {
	/// Poison every store of the process for `reason`. Returns whether this call poisoned
	/// it (the first).
	pub fn set(&self, reason: impl Into<String>) -> bool {
		let first = self.reason.set(reason.into()).is_ok();
		self.poisoned.store(true, Ordering::Release);
		if first {
			let reason = self.reason.get().cloned();
			self.tx.send_replace(reason);
		}
		first
	}

	/// Why the process is poisoned, if it is.
	#[must_use]
	pub fn get(&self) -> Option<String> {
		self.poisoned.load(Ordering::Acquire).then(|| self.reason.get().cloned().unwrap_or_default())
	}

	/// A watch of the poison: `None` until it is set, then its reason.
	#[must_use]
	pub fn subscribe(&self) -> watch::Receiver<Option<String>> {
		self.tx.subscribe()
	}
}
