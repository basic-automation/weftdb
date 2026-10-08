//! The control-plane write scope (robustness track ROB-2).
//!
//! A panic in the middle of a control-plane write is not like a panic in a read: Turso's
//! pager may be half way through a commit, and its locks do not poison. So the process's
//! panic hook needs to know whether the panicking task was writing the control plane.
//! [`control_plane_write`] runs a future with the task-local [`CONTROL_PLANE_WRITE`] set,
//! and the hook, which runs synchronously inside the poll that panicked, reads it with
//! `CONTROL_PLANE_WRITE.try_with(|f| f.get())` (or [`in_control_plane_write`]). Every
//! segment-index transaction (`IndexTxn`, the store's whole `segment_index.db` write
//! path) runs inside it; a hook that finds the flag set write-poisons the process
//! ([`poison_global`](crate::durable::poison_global)) instead of letting it go on writing
//! through that pager.

use std::{cell::Cell, future::Future};

tokio::task_local! {
	/// Set to `true` for the duration of a [`control_plane_write`]; unset elsewhere.
	pub static CONTROL_PLANE_WRITE: Cell<bool>;
}

/// Run `fut` as a control-plane write: with [`CONTROL_PLANE_WRITE`] set while it is
/// polled.
pub async fn control_plane_write<F: Future>(fut: F) -> F::Output {
	CONTROL_PLANE_WRITE.scope(Cell::new(true), fut).await
}

/// Whether the current task is inside a [`control_plane_write`]. `false` outside a task
/// or outside the scope.
#[must_use]
pub fn in_control_plane_write() -> bool {
	CONTROL_PLANE_WRITE.try_with(Cell::get).unwrap_or(false)
}

#[cfg(test)]
tokio::task_local! {
	/// Test-only: while set, every [`IndexTxn`](crate::types::index_txn::IndexTxn) op
	/// records whether it ran inside a [`control_plane_write`], so a test can see the
	/// scope from inside the transaction.
	pub(crate) static OBSERVED_SCOPES: std::cell::RefCell<Vec<bool>>;
}

/// Record [`in_control_plane_write`] into [`OBSERVED_SCOPES`], when a test set it.
#[cfg(test)]
pub(crate) fn observe_scope() {
	let inside = in_control_plane_write();
	// Outside a test's scope there is nothing to record into.
	let _ = OBSERVED_SCOPES.try_with(|seen| seen.borrow_mut().push(inside));
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn the_scope_is_set_only_inside_a_control_plane_write() {
		assert!(!in_control_plane_write(), "a task outside any scope");
		let inside = control_plane_write(async {
			tokio::task::yield_now().await;
			in_control_plane_write()
		})
		.await;
		assert!(inside, "set across an await inside the scope");
		assert!(!in_control_plane_write(), "and unset again after it");
		let spawned = control_plane_write(async { tokio::spawn(async { in_control_plane_write() }).await.expect("joins") }).await;
		assert!(!spawned, "a task spawned from the scope is not inside it");
	}
}
