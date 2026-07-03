//! Automatic background out-of-order reconciliation (roadmap **Phase 4.6**).
//!
//! The manual `POST /api/v1/storage/{aspect}/reconcile` endpoint (see
//! [`manage`](crate::manage)) lets an operator trigger a reconciliation pass, and
//! its `?threshold=N` form gates that pass on the `unsorted_segments` order-health
//! backlog. This module turns that same trigger into a **background timer**: on an
//! interval it sweeps every declared aspect through
//! [`SegmentStore::reconcile_all_over_threshold`](database::SegmentStore::reconcile_all_over_threshold),
//! paying the rewrite only for the aspects whose backlog has crossed the threshold.
//!
//! This is the QuestDB-style automatic squash: rather than rewriting on every late
//! row, the backlog is allowed to accumulate and is squashed only once it grows past
//! a configured split count. Each reconciled aspect is counted as a pass in the
//! `dsp_reconcile_*` metrics, exactly like a manual trigger, so an operator can watch
//! how often the daemon fires and how much it rewrites and tune the threshold and
//! interval against it.
//!
//! The daemon holds only `Arc` handles (the store and the metrics registry), so it
//! is a detached side task; the router and its handlers are untouched.

use std::{sync::Arc, time::Duration};

use database::{ReconcileSweep, SegmentStore};

use crate::metrics::SharedMetrics;

/// Configuration for the background reconcile daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileDaemonConfig {
	/// How often the daemon sweeps the store.
	pub interval: Duration,
	/// The `unsorted_segments` backlog an aspect must reach before the sweep
	/// reconciles it (clamped to 1 by the store trigger).
	pub threshold: usize,
}

/// Run one reconcile tick.
///
/// Sweeps every aspect whose out-of-order backlog is at or above `threshold`,
/// recording the reconciled aspects and segments in `metrics`, and returns the
/// [`ReconcileSweep`] summary.
///
/// Factored out of the timer loop so the per-tick behaviour is unit-testable without
/// waiting on a real interval. Records nothing when the sweep reconciled nothing, so
/// a quiet tick leaves the `dsp_reconcile_*` counters unchanged.
///
/// # Errors
///
/// Propagates a failure from
/// [`SegmentStore::reconcile_all_over_threshold`](database::SegmentStore::reconcile_all_over_threshold)
/// (a control-plane read, a segment read, or a re-seal failure).
pub async fn reconcile_tick(store: &SegmentStore, metrics: &SharedMetrics, threshold: usize) -> anyhow::Result<ReconcileSweep> {
	let sweep = store.reconcile_all_over_threshold(threshold).await?;
	if sweep.aspects_reconciled > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled).unwrap_or(u64::MAX));
	}
	Ok(sweep)
}

/// Spawn the background reconcile daemon on `config.interval`.
///
/// Returns the task handle; dropping it leaves the daemon running detached for the
/// process lifetime, which is the intended deployment shape.
///
/// The first sweep fires one interval after start (the immediate `interval` tick is
/// consumed first), so start-up is not stampeded by an eager pass. Each sweep that
/// reconciles at least one aspect logs a one-line summary; a sweep failure is logged
/// and the loop continues (a transient control-plane error must not kill the daemon).
#[must_use]
pub fn spawn_reconcile_daemon(store: Arc<SegmentStore>, metrics: SharedMetrics, config: ReconcileDaemonConfig) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		let mut ticker = tokio::time::interval(config.interval);
		// Consume the immediate first tick so the first sweep waits one interval.
		ticker.tick().await;
		loop {
			ticker.tick().await;
			match reconcile_tick(&store, &metrics, config.threshold).await {
				Ok(sweep) if sweep.aspects_reconciled > 0 => {
					println!("reconcile daemon: scanned {} aspect(s), reconciled {} aspect(s) / {} segment(s)", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled);
				},
				Ok(_) => {},
				Err(err) => eprintln!("reconcile daemon: sweep failed: {err}"),
			}
		}
	})
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use bigdecimal::BigDecimal;
	use database::SegmentStore;
	use dsp_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use tempfile::TempDir;

	use super::*;
	use crate::metrics::Metrics;

	fn schema() -> AspectSchema {
		AspectSchema::new(PhysicalType::F64, "0".parse().unwrap(), TimeUnit::Seconds)
	}

	fn bd(s: &str) -> BigDecimal {
		s.parse().unwrap()
	}

	#[tokio::test]
	async fn tick_reconciles_over_threshold_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		store.declare("b", &schema()).await.unwrap();
		// a: backlog 2 (two out-of-order segments); b: backlog 1.
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.unwrap();
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.unwrap();
		store.seal("b", &schema(), &[100_i64, 130, 110], &[bd("7"), bd("9"), bd("8")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		// One tick at threshold 2: only aspect a fires (2 passes = 1 aspect? no —
		// a is one reconciled aspect rewriting 2 segments). b holds below threshold.
		let sweep = reconcile_tick(&store, &metrics, 2).await.unwrap();
		let snap = metrics.snapshot();
		assert_eq!(sweep.aspects_reconciled, 1, "only a is over threshold 2");
		assert_eq!(sweep.segments_reconciled, 2, "both of a's segments rewritten");
		assert_eq!(snap.reconcile.passes, 1, "one aspect reconciled = one pass");
		assert_eq!(snap.reconcile.segments_reconciled, 2);
		assert_eq!(store.aspect_stats("b").await.unwrap().unsorted_segments, 1, "b untouched");

		// A quiet tick (nothing over threshold 2 now) records nothing.
		let quiet = reconcile_tick(&store, &metrics, 2).await.unwrap();
		let snap2 = metrics.snapshot();
		drop(store);
		assert_eq!(quiet.aspects_reconciled, 0);
		assert_eq!(snap2.reconcile.passes, 1, "the quiet tick did not bump the pass counter");
	}
}
