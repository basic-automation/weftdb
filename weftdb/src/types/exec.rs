//! The execution scopes the process's panic handling keys off (robustness
//! track ROB-2 and ROB-3, release plan C12): this module is the
//! `weftdb::exec` entry point — the one place a host runs work whose panics
//! it means to classify, rather than a place that decides what a panic costs.
//!
//! ## The control-plane write scope (ROB-2)
//!
//! A panic in the middle of a control-plane write is not like a panic in a read: Turso's
//! pager may be half way through a commit, and its locks do not poison. So the process's
//! panic hook needs to know whether the panicking task was writing the control plane.
//! [`control_plane_write`] runs a future with the task-local [`CONTROL_PLANE_WRITE`] set,
//! and the hook, which runs synchronously inside the poll that panicked, reads it with
//! `CONTROL_PLANE_WRITE.try_with(|f| f.get())` (or [`in_control_plane_write`]). Every
//! segment-index transaction (`IndexTxn`, the store's whole `segment_index.db` write
//! path) runs inside it; a hook that finds the flag set write-poisons the process
//! ([`poison_global`](crate::durable::poison_global)) and stops the process from going on
//! writing through that pager — `weft-server`'s hook does exactly that and aborts (ROB-2).
//! The scope is a task-local, not a thread-local, so it follows the task across `await`
//! points and worker threads. It is **not** inherited by `tokio::spawn`, which is why the
//! region wraps whole operations rather than individual statements: work a transaction
//! spawned out of the region would run unmarked, and the tests below check that.
//!
//! ## The ingest phase scope and the panic catch (ROB-3)
//!
//! ROB-3's catch is the only layer that catches panics (plan C12; the
//! CatchPanicLayer FRE-6a proposed was dropped in X-ERR for exactly this).
//! [`catch`] is that catch's library half: it runs a future under
//! `catch_unwind` and, when the future panics, hands the caller a
//! [`CaughtPanic`] — the payload rendered safely, and the
//! [`IngestPhase`](super::ingest::IngestPhase) the task had reached —
//! instead of letting the unwind escape. Two things it deliberately is not:
//!
//! - It does not rescue a control-plane panic. The ROB-2 hook runs *during*
//!   the unwind, before `catch_unwind` observes it, and aborts the process
//!   when the flag above is set; `catch` never sees those panics, which is
//!   what "ROB-2's abort is preserved" means mechanically.
//! - It does not choose the response. Mapping a caught panic to a status is
//!   the host's policy (in `weft-server`: by `IngestPhase`, plan D21 —
//!   before [`Committing`](super::ingest::IngestPhase::Committing) the write
//!   did not commit, at or after it the outcome is unknown).
//!
//! The phase is a task-local like the write scope: [`ingest_phase_scope`]
//! opens it at [`Validating`](super::ingest::IngestPhase::Validating),
//! [`SegmentStore::ingest`](crate::SegmentStore::ingest) advances it with
//! [`set_ingest_phase`] as it advances its own local phase, and [`catch`]
//! reads it back with [`current_ingest_phase`] while the scope is still
//! open — which is why a caller wraps `catch`, never the other way round:
//! `ingest_phase_scope(catch(work))`.

use std::{any::Any, cell::Cell, future::Future, panic::AssertUnwindSafe};

use futures::FutureExt as _;

use super::ingest::IngestPhase;

tokio::task_local! {
	/// Set to `true` for the duration of a [`control_plane_write`]; unset elsewhere.
	pub static CONTROL_PLANE_WRITE: Cell<bool>;

	/// The phase the current task's ingest has reached, for the duration of
	/// an [`ingest_phase_scope`]; `None` before the scope opens an ingest
	/// and outside it.
	static INGEST_PHASE: Cell<Option<IngestPhase>>;
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

/// Run `fut` with the ingest phase scope open, starting at
/// [`IngestPhase::Validating`]. See the module docs for why a panic catch
/// belongs *inside* this scope.
pub async fn ingest_phase_scope<F: Future>(fut: F) -> F::Output {
	INGEST_PHASE.scope(Cell::new(Some(IngestPhase::Validating)), fut).await
}

/// Advance the current task's ingest phase. A no-op outside an
/// [`ingest_phase_scope`], so library code on a path nobody scoped (a test,
/// an embedding host that does not map panics) pays nothing for it.
pub fn set_ingest_phase(phase: IngestPhase) {
	let _ = INGEST_PHASE.try_with(|cell| cell.set(Some(phase)));
}

/// The phase the current task's ingest has reached, or `None` outside an
/// [`ingest_phase_scope`].
#[must_use]
pub fn current_ingest_phase() -> Option<IngestPhase> {
	INGEST_PHASE.try_with(Cell::get).unwrap_or(None)
}

/// A panic [`catch`] caught: what it said, and where its ingest had got to.
///
/// The payload is rendered at catch time by [`panic_message`], because the
/// payload object itself does not outlive the unwind usefully — and because
/// rendering it later, in code that is not braced for a payload whose
/// `Display` panics, would be a second fault on top of the first.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaughtPanic {
	/// The panic's payload as text (`&str`/`String` payloads verbatim; any
	/// other payload named, never formatted — see [`panic_message`]).
	pub message: String,
	/// The [`IngestPhase`] the panicking task had reached, when it was
	/// inside an [`ingest_phase_scope`]; `None` otherwise. ROB-3's mapping
	/// (plan D21) keys off this: see the module docs.
	pub ingest_phase: Option<IngestPhase>,
}

/// Render a panic payload as text without ever formatting an arbitrary
/// payload: only `&str` and `String` payloads — the kinds `panic!` itself
/// produces — are shown; anything else is reported as unprintable rather
/// than risking a `Display` impl that panics inside panic handling.
#[must_use]
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
	if let Some(text) = payload.downcast_ref::<&str>() {
		(*text).to_owned()
	} else if let Some(text) = payload.downcast_ref::<String>() {
		text.clone()
	} else {
		"a panic whose payload could not be printed".to_owned()
	}
}

/// Run `fut` and catch any panic it raises, as a [`CaughtPanic`].
///
/// This is the `weftdb::exec` entry point ROB-3's containment is built on:
/// `weft-server`'s single panic-catch layer runs every request through it,
/// and daemon supervision runs through the same classification (a
/// `JoinError` there, a [`CaughtPanic`] here — both name the phase and the
/// message, neither invents a second catch).
///
/// A panic inside a [`control_plane_write`] never reaches the `Err` arm:
/// the host's ROB-2 hook aborts during the unwind first (plan C12/C22).
///
/// # Errors
///
/// [`CaughtPanic`] when `fut` panics. A `fut` that returns normally —
/// including one that returns its own `Err` — is `Ok` here, untouched.
pub async fn catch<F: Future>(fut: F) -> Result<F::Output, CaughtPanic> {
	match AssertUnwindSafe(fut).catch_unwind().await {
		Ok(output) => Ok(output),
		Err(payload) => Err(CaughtPanic { message: panic_message(&*payload), ingest_phase: current_ingest_phase() }),
	}
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

	#[tokio::test]
	async fn catch_returns_the_output_of_a_future_that_does_not_panic() {
		let caught = catch(async { 41 + 1 }).await;
		assert_eq!(caught, Ok(42));
	}

	#[tokio::test]
	async fn catch_reports_the_message_and_the_phase_reached() {
		let caught = ingest_phase_scope(catch(async {
			set_ingest_phase(IngestPhase::Committing);
			tokio::task::yield_now().await;
			panic!("the pager sneezed");
		}))
		.await
		.expect_err("the panic is caught, not propagated");
		assert_eq!(caught.message, "the pager sneezed");
		assert_eq!(caught.ingest_phase, Some(IngestPhase::Committing), "D21 mapping keys off the phase at panic time");
	}

	#[tokio::test]
	async fn catch_outside_a_phase_scope_reports_no_phase() {
		let caught = catch(async { panic!("plain failure") }).await.expect_err("caught");
		assert_eq!(caught.message, "plain failure");
		assert_eq!(caught.ingest_phase, None);
	}

	#[tokio::test]
	async fn the_phase_starts_at_validating_and_advances_only_inside_the_scope() {
		assert_eq!(current_ingest_phase(), None, "no scope, no phase");
		set_ingest_phase(IngestPhase::Committing);
		assert_eq!(current_ingest_phase(), None, "setting outside a scope is a no-op");
		let seen = ingest_phase_scope(async {
			let start = current_ingest_phase();
			set_ingest_phase(IngestPhase::Encoding);
			(start, current_ingest_phase())
		})
		.await;
		assert_eq!(seen, (Some(IngestPhase::Validating), Some(IngestPhase::Encoding)));
		assert_eq!(current_ingest_phase(), None, "the scope ends with the future");
	}

	#[tokio::test]
	async fn a_non_string_payload_is_named_not_formatted() {
		let caught = catch(async { std::panic::panic_any(7_u8) }).await.expect_err("caught");
		assert_eq!(caught.message, "a panic whose payload could not be printed");
	}
}
