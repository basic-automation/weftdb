//! The router's shared application state.
//!
//! Until now every handler took the bare [`SharedMetrics`] as its router state.
//! The storage-query endpoints (roadmap Phase 2 — raw range / stored queries)
//! need a second long-lived dependency: a [`weftdb::SegmentStore`] reading the
//! on-disk Storage v2 segments. Rather than thread two parallel `State` extractors
//! through the router, the service now carries one [`AppState`] and exposes its
//! parts through [`axum::extract::FromRef`].
//!
//! ## Why `FromRef` (and not a flag-day rewrite)
//!
//! Every existing handler keeps extracting `State<SharedMetrics>` **unchanged**:
//! the [`FromRef<AppState>`] impl lets axum project the metrics handle out of the
//! combined state for them. Only the new storage handlers extract the whole
//! [`AppState`] (they need the optional store). This keeps the metrics/interpolate/
//! downsample handlers and their tests untouched while the router gains a second
//! dependency.
//!
//! ## The store is optional on purpose
//!
//! A segment store is only present when the operator configures a store root (see
//! the binary's `WEFT_SEGMENT_STORE_ROOT`). With no root the server still serves
//! the stateless interpolation/downsample API exactly as before, and the storage
//! endpoints answer `503 Service Unavailable` — so `app()` (and the whole existing
//! test suite) runs with no filesystem state.

use std::sync::Arc;

use axum::extract::FromRef;
use weftdb::SegmentStore;

use crate::metrics::SharedMetrics;

/// The router state shared by every handler: the process metrics plus an optional
/// segment store backing the storage-query endpoints.
///
/// Cheap to clone — both fields are `Arc`-backed handles.
#[derive(Clone, Default)]
pub struct AppState {
	/// Process-lifetime counters (`/metrics`), shared with the capability handlers.
	metrics: SharedMetrics,
	/// The on-disk segment store, present only when a store root is configured.
	store: Option<Arc<SegmentStore>>,
}

impl AppState {
	/// Build state with a fresh metrics registry and no segment store — the shape
	/// the stateless API (and the existing tests) run under.
	#[must_use]
	pub fn new() -> Self {
		Self::default()
	}

	/// Build state over a caller-supplied [`SharedMetrics`] (so a test can observe
	/// the counters) and no segment store.
	#[must_use]
	pub const fn with_metrics(metrics: SharedMetrics) -> Self {
		Self { metrics, store: None }
	}

	/// Attach a segment store, enabling the storage-query endpoints.
	#[must_use]
	pub fn with_store(mut self, store: Arc<SegmentStore>) -> Self {
		self.store = Some(store);
		self
	}

	/// The shared metrics handle.
	#[must_use]
	pub const fn metrics(&self) -> &SharedMetrics {
		&self.metrics
	}

	/// The configured segment store, or [`None`] when no store root is set.
	#[must_use]
	pub const fn store(&self) -> Option<&Arc<SegmentStore>> {
		self.store.as_ref()
	}
}

impl FromRef<AppState> for SharedMetrics {
	fn from_ref(state: &AppState) -> Self {
		state.metrics.clone()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_state_has_metrics_and_no_store() {
		let state = AppState::new();
		assert!(state.store().is_none());
		// The metrics handle is real and usable.
		state.metrics().record_interpolate_request();
		let requests = state.metrics().snapshot().interpolate.requests;
		drop(state);
		assert_eq!(requests, 1);
	}

	#[test]
	fn from_ref_projects_the_metrics_handle() {
		let metrics: SharedMetrics = SharedMetrics::default();
		let state = AppState::with_metrics(metrics.clone());
		// FromRef hands back a clone of the same Arc, so a count through one is
		// visible through the other.
		let projected = SharedMetrics::from_ref(&state);
		drop(state);
		projected.record_interpolate_request();
		assert_eq!(metrics.snapshot().interpolate.requests, 1);
	}
}
