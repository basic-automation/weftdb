//! Cooperative cancellation for maintenance sweeps (`ROB-21a`).
//!
//! The [`MaintenanceCancel`] token is a lightweight `Arc<AtomicBool>` that a caller
//! hands to a store-wide sweep. A cancellation request prevents the sweep from starting
//! the *next* aspect's maintenance; it never interrupts an aspect already in flight.
//! The check runs at each aspect boundary, before the aspect's maintenance guard is
//! acquired (the same seam where [`breaker_permits`] is checked).

use std::sync::{
	atomic::{AtomicBool, Ordering},
	Arc,
};

/// A cooperative cancellation token for store-wide maintenance sweeps.
///
/// A call to [`request`](Self::request) sets a flag that the next `is_requested()`
/// check in a sweep loop observes. The check runs **between** aspects, before the
/// aspect's maintenance guard is taken, so:
///
/// - The in-flight aspect runs to its natural end and is counted normally.
/// - A swap (`commit_plans` / `finish_swap`) is never interrupted — the existing
///   fault-injection matrix (11 fault points, 5 abort points) and the
///   [`PendingOutputs`] → [`crate::Reaper::abandon`] cleanup chain remain valid.
/// - A fine-grained token threaded into read paths is **not** provided — that is
///   deferred to ROB-15.
///
/// The type is `Clone`, `Send` and `Sync` (the underlying [`Arc`] is); every clone
/// shares the same flag. [`Default`] creates a fresh, unrequested token.
///
/// # Examples
///
/// ```rust,ignore
/// use weftdb::MaintenanceCancel;
/// let token = MaintenanceCancel::default();
/// assert!(!token.is_requested());
/// token.request();
/// assert!(token.is_requested());
/// ```
#[derive(Clone, Default, Debug)]
pub struct MaintenanceCancel(pub(crate) Arc<AtomicBool>);

impl MaintenanceCancel {
	/// Request cancellation. Subsequent calls to [`is_requested`](Self::is_requested)
	/// on any clone of this token return `true`.
	pub fn request(&self) {
		self.0.store(true, Ordering::Release);
	}

	/// Whether a cancellation has been requested.
	///
	/// The sweep loop polls this once per aspect, before acquiring the aspect's
	/// maintenance guard.
	#[must_use]
	pub fn is_requested(&self) -> bool {
		self.0.load(Ordering::Acquire)
	}
}