//! Automatic background out-of-order reconciliation (roadmap **Phase 4.6**).
//!
//! The manual `POST /api/v1/storage/{aspect}/reconcile` endpoint (see
//! [`manage`](crate::manage)) lets an operator trigger a reconciliation pass, and
//! its `?threshold=N` form gates that pass on the `unsorted_segments` order-health
//! backlog. This module turns that same trigger into a **background timer**: on an
//! interval it sweeps every declared aspect through
//! [`SegmentStore::reconcile_all_over_threshold`](weftdb::SegmentStore::reconcile_all_over_threshold),
//! paying the rewrite only for the aspects whose backlog has crossed the threshold.
//!
//! This is the QuestDB-style automatic squash: rather than rewriting on every late
//! row, the backlog is allowed to accumulate and is squashed only once it grows past
//! a configured split count. Each reconciled aspect is counted as a pass in the
//! `weft_reconcile_*` metrics, exactly like a manual trigger, so an operator can watch
//! how often the daemon fires and how much it rewrites and tune the threshold and
//! interval against it.
//!
//! **Sweep isolation (crash-consistency S4).** The store-wide sweeps visit every
//! aspect and return the ones that failed in their `failed` list instead of stopping at
//! the first error, so one torn frame no longer stalls maintenance of every aspect after
//! it in name order. Each tick logs one `WARN` line per failed aspect (naming the sweep
//! kind, the aspect and the full error chain) and counts it in
//! `weft_reconcile_failed_passes_total`; the healthy aspects are maintained as usual.
//!
//! **Busy aspects (crash-consistency S7).** Maintenance operations on one aspect take
//! turns under the aspect's maintenance lock. The daemon never waits for it: every sweep
//! runs with [`MaintenanceWait::Skip`], so an aspect an operator's request (or a long
//! pass) holds is left for the next tick, logged at `DEBUG` and recorded in the tick
//! span's `busy` field, and the HTTP endpoints, which do wait, never queue behind a tick.
//!
//! **Write-once passes (crash-consistency S8, S9).** Every pass a tick runs (a reconcile,
//! an overlap merge, a squash, a compaction) writes its outputs to new, fsynced frames and
//! swaps them in with one `segment_index.db` transaction per segment, overlap component or
//! compaction group, so a crash, power cut or kill in the middle of a tick leaves each of
//! them as it was or as the pass left it, never torn or half-merged, and the frames a swap
//! replaced are deleted by the store's reaper once no read can still be using them. A
//! pass stopped part way (its task dropped at shutdown) leaves outputs that the next pass
//! on the aspect, or the next start's journal replay, removes.
//!
//! The daemon holds only `Arc` handles (the store and the metrics registry), so it
//! is a detached side task; the router and its handlers are untouched.

use std::{sync::Arc, time::Duration};

use tracing::Instrument as _;
use weftdb::{HotColdSweep, MaintenanceWait, OverlapSweep, ReconcileSweep, SegmentStore, SquashSweep};

use crate::metrics::SharedMetrics;

/// Log and count the aspects a store-wide sweep could not maintain.
///
/// The sweeps record a failing aspect and move on (crash-consistency S4), so a failure
/// no longer surfaces as the tick's `Err`. Without this, an aspect that fails on every
/// tick (a truncated frame, say) would be skipped silently forever. Each failure is
/// logged as exactly one `WARN` line inside the tick's span, carrying the sweep kind, the
/// aspect and the error with its full cause chain, and the count is added to
/// `weft_reconcile_failed_passes_total`. A sweep with no failures logs nothing and
/// leaves the counter unchanged.
fn report_sweep_failures(span: &tracing::Span, metrics: &SharedMetrics, kind: &'static str, failed: &[(String, anyhow::Error)]) {
	span.record("failed", failed.len());
	if failed.is_empty() {
		return;
	}
	span.in_scope(|| {
		for (aspect, err) in failed {
			let error = format!("{err:#}");
			tracing::warn!(sweep = kind, aspect = %aspect, error = %error, "reconcile daemon: aspect pass failed; the sweep skipped it and continued");
		}
	});
	metrics.record_reconcile_failures(u64::try_from(failed.len()).unwrap_or(u64::MAX));
}

/// Record the aspects a sweep skipped because another maintenance operation held them:
/// the tick span's `busy` field, and one `DEBUG` line each. A busy aspect is not a
/// failure (it is maintained on a later tick), so it is not counted as one.
fn report_busy_aspects(span: &tracing::Span, kind: &'static str, busy: &[String]) {
	span.record("busy", busy.len());
	span.in_scope(|| {
		for aspect in busy {
			tracing::debug!(sweep = kind, aspect = %aspect, "reconcile daemon: aspect busy with another maintenance operation; skipped this tick");
		}
	});
}

/// Configuration for the background reconcile daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileDaemonConfig {
	/// How often the daemon sweeps the store.
	pub interval: Duration,
	/// The `unsorted_segments` backlog an aspect must reach before the sweep
	/// reconciles it (clamped to 1 by the store trigger).
	pub threshold: usize,
	/// When `true`, sweep in **hot/cold** mode: reconcile every aspect's cold
	/// (sealed, no-longer-appended) segments on each tick and defer only the
	/// hot tail until the backlog reaches `threshold` (see
	/// [`reconcile_tick_hot_cold`] /
	/// [`SegmentStore::reconcile_all_hot_cold`](weftdb::SegmentStore::reconcile_all_hot_cold)).
	/// When `false`, use the all-or-nothing threshold sweep (the original
	/// [`reconcile_tick`] behaviour).
	pub hot_cold: bool,
	/// When `true`, each tick **also** runs a store-wide cross-segment overlap merge
	/// ([`reconcile_tick_overlaps`] / [`SegmentStore::reconcile_all_overlaps`](weftdb::SegmentStore::reconcile_all_overlaps))
	/// after the intra-segment sweep, so late data that re-entered an already-covered
	/// window is merged in the background too. Independent of `hot_cold`/`threshold`.
	pub overlaps: bool,
	/// Optional split-not-rewrite floor in **bytes** for the overlap merge (roadmap
	/// Phase 4.6). When `Some` and `overlaps` is set, each overlap tick runs under a
	/// [`SplitPolicy`](weft_physical_type::SplitPolicy) with this floor, so an aspect's
	/// dominant cold prefix is split off rather than fully rewritten. When `None`, the
	/// merge uses the default 50 MiB floor (small components always full-rewrite).
	pub split_min_bytes: Option<u64>,
	/// Optional segment-count cap (roadmap Phase 4.6). When `Some`, each tick **also**
	/// squashes every aspect whose segment count exceeds it into one segment
	/// ([`reconcile_tick_squash`] / [`SegmentStore::squash_all_over_threshold`](weftdb::SegmentStore::squash_all_over_threshold)),
	/// bounding the fragmentation repeated split carve-offs create. When `None`, no
	/// squash runs. Independent of `overlaps`/`hot_cold`/`threshold`.
	pub squash_max_segments: Option<usize>,
	/// Optional **target segment size in rows** for size-aware compaction (roadmap
	/// Phase 4.6). When `Some`, each tick **also** coalesces every aspect's segments
	/// toward ~this many rows per segment
	/// ([`reconcile_tick_compact`] / [`SegmentStore::squash_all_to_target_rows`](weftdb::SegmentStore::squash_all_to_target_rows)),
	/// holding fragmentation near the read-optimal size rather than folding to one (which
	/// `squash_max_segments` does). When `None`, no size-aware compaction runs. Independent
	/// of the other axes; runs after the squash pass.
	pub compact_target_rows: Option<usize>,
}

/// Run one reconcile tick.
///
/// Sweeps every aspect whose out-of-order backlog is at or above `threshold`,
/// recording the reconciled aspects and segments in `metrics`, and returns the
/// [`ReconcileSweep`] summary.
///
/// Factored out of the timer loop so the per-tick behaviour is unit-testable without
/// waiting on a real interval. Records nothing when the sweep reconciled nothing, so
/// a quiet tick leaves the `weft_reconcile_*` counters unchanged.
///
/// An aspect whose pass failed (a segment read or re-seal failure) is skipped by the
/// sweep, logged as one `WARN` line and counted in `weft_reconcile_failed_passes_total`;
/// it is also returned in [`ReconcileSweep::failed`](weftdb::ReconcileSweep::failed).
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::reconcile_all_over_threshold`](weftdb::SegmentStore::reconcile_all_over_threshold).
pub async fn reconcile_tick(store: &SegmentStore, metrics: &SharedMetrics, threshold: usize) -> anyhow::Result<ReconcileSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "threshold", threshold, aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	let sweep = store.reconcile_all_over_threshold(threshold, MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_reconciled);
	span.record("segments", sweep.segments_reconciled);
	if sweep.aspects_reconciled > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "threshold", &sweep.failed);
	report_busy_aspects(&span, "threshold", &sweep.busy);
	Ok(sweep)
}

/// Run one **hot/cold** reconcile tick.
///
/// Sweeps every aspect through
/// [`SegmentStore::reconcile_all_hot_cold`](weftdb::SegmentStore::reconcile_all_hot_cold):
/// each aspect's cold segments are reconciled unconditionally and only its hot tail
/// is gated on `threshold`. Records the reconciled aspects and total (cold + hot)
/// segments in `metrics`, and returns the [`HotColdSweep`] summary.
///
/// As with [`reconcile_tick`], a sweep that reconciled nothing records nothing, so a
/// quiet tick leaves the `weft_reconcile_*` counters unchanged, and a failed aspect is
/// skipped, logged as one `WARN` line and counted in `weft_reconcile_failed_passes_total`.
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::reconcile_all_hot_cold`](weftdb::SegmentStore::reconcile_all_hot_cold).
pub async fn reconcile_tick_hot_cold(store: &SegmentStore, metrics: &SharedMetrics, threshold: usize) -> anyhow::Result<HotColdSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "hot_cold", threshold, aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	let sweep = store.reconcile_all_hot_cold(threshold, MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_reconciled);
	span.record("segments", sweep.segments_reconciled());
	if sweep.aspects_reconciled > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_reconciled()).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "hot_cold", &sweep.failed);
	report_busy_aspects(&span, "hot_cold", &sweep.busy);
	Ok(sweep)
}

/// Run one **cross-segment overlap** merge tick.
///
/// Sweeps every aspect through
/// [`SegmentStore::reconcile_all_overlaps`](weftdb::SegmentStore::reconcile_all_overlaps),
/// merging each aspect's time-overlap groups into single segments. Records the
/// reconciled aspects and total segments removed in `metrics` (a merge is a pass,
/// exactly like an intra-segment reconcile), and returns the [`OverlapSweep`]. A sweep
/// that merged nothing records nothing. A failed aspect is skipped, logged as one `WARN`
/// line and counted in `weft_reconcile_failed_passes_total`.
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::reconcile_all_overlaps`](weftdb::SegmentStore::reconcile_all_overlaps).
pub async fn reconcile_tick_overlaps(store: &SegmentStore, metrics: &SharedMetrics) -> anyhow::Result<OverlapSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "overlaps", aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	let sweep = store.reconcile_all_overlaps(MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_reconciled);
	span.record("segments", sweep.segments_removed);
	if sweep.aspects_reconciled > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "overlaps", &sweep.failed);
	report_busy_aspects(&span, "overlaps", &sweep.busy);
	Ok(sweep)
}

/// Run one **cross-segment overlap** merge tick under an explicit split floor
/// (roadmap Phase 4.6 — the split-not-rewrite path).
///
/// As [`reconcile_tick_overlaps`],
/// but each aspect's merge runs through
/// [`SegmentStore::reconcile_all_overlaps_with_policy`](weftdb::SegmentStore::reconcile_all_overlaps_with_policy)
/// under `SplitPolicy::new(min_split_bytes)`, so a dominant cold prefix is split off
/// rather than fully rewritten. Records and returns exactly as
/// [`reconcile_tick_overlaps`].
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::reconcile_all_overlaps_with_policy`](weftdb::SegmentStore::reconcile_all_overlaps_with_policy).
pub async fn reconcile_tick_overlaps_with_policy(store: &SegmentStore, metrics: &SharedMetrics, min_split_bytes: u64) -> anyhow::Result<OverlapSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "overlaps_split", min_split_bytes, aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	let sweep = store.reconcile_all_overlaps_with_policy(weft_physical_type::SplitPolicy::new(min_split_bytes), MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_reconciled);
	span.record("segments", sweep.segments_removed);
	if sweep.aspects_reconciled > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_reconciled).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "overlaps_split", &sweep.failed);
	report_busy_aspects(&span, "overlaps_split", &sweep.busy);
	Ok(sweep)
}

/// Run one **squash** tick (roadmap Phase 4.6 — the squash half of the
/// split-not-rewrite path).
///
/// Sweeps every aspect through
/// [`SegmentStore::squash_all_over_threshold`](weftdb::SegmentStore::squash_all_over_threshold),
/// folding each aspect whose segment count exceeds `max_segments` into one segment —
/// the bound on the fragmentation repeated split carve-offs create. Records the
/// squashed aspects and removed segments in `metrics` (a squash is a pass, like a
/// reconcile), and returns the [`SquashSweep`]. A sweep that squashed nothing records
/// nothing. A failed aspect is skipped, logged as one `WARN` line and counted in
/// `weft_reconcile_failed_passes_total`.
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::squash_all_over_threshold`](weftdb::SegmentStore::squash_all_over_threshold).
pub async fn reconcile_tick_squash(store: &SegmentStore, metrics: &SharedMetrics, max_segments: usize) -> anyhow::Result<SquashSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "squash", max_segments, aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	let sweep = store.squash_all_over_threshold(max_segments, MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_squashed);
	span.record("segments", sweep.segments_removed);
	if sweep.aspects_squashed > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_squashed).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "squash", &sweep.failed);
	report_busy_aspects(&span, "squash", &sweep.busy);
	Ok(sweep)
}

/// Run one **size-targeted compaction** tick (roadmap Phase 4.6).
///
/// Sweeps every aspect through the **fragmentation-gated**
/// [`SegmentStore::squash_all_to_target_rows_if_fragmented`](weftdb::SegmentStore::squash_all_to_target_rows_if_fragmented),
/// coalescing each *over-fragmented* aspect's segments toward ~`target_rows` rows per segment
/// (leaving already-well-sized aspects untouched via an O(1) rollup check, no segment-index
/// scan) — the size-aware bound on fragmentation, motivated by the `downsample_range` knee
/// (one giant segment reads slower than several mid-sized ones, so [`reconcile_tick_squash`]'s
/// fold-to-one over-corrects). Records the compacted aspects and removed segments in `metrics`
/// (a compaction is a pass, like a squash), and returns the [`SquashSweep`]. A sweep that
/// coalesced nothing records nothing. A failed aspect is skipped, logged as one `WARN`
/// line and counted in `weft_reconcile_failed_passes_total`.
///
/// # Errors
///
/// Propagates a failure to list the store's aspects from
/// [`SegmentStore::squash_all_to_target_rows_if_fragmented`](weftdb::SegmentStore::squash_all_to_target_rows_if_fragmented).
pub async fn reconcile_tick_compact(store: &SegmentStore, metrics: &SharedMetrics, target_rows: usize) -> anyhow::Result<SquashSweep> {
	let span = tracing::info_span!("reconcile.tick", kind = "compact", target_rows, aspects = tracing::field::Empty, segments = tracing::field::Empty, failed = tracing::field::Empty, busy = tracing::field::Empty);
	// The gated sweep: a converged aspect costs one O(1) rollup read per tick, not a full
	// segment-index scan, so the 1s daemon does not churn the control plane on a tidy store.
	let sweep = store.squash_all_to_target_rows_if_fragmented(target_rows, MaintenanceWait::Skip).instrument(span.clone()).await?;
	span.record("aspects", sweep.aspects_squashed);
	span.record("segments", sweep.segments_removed);
	if sweep.aspects_squashed > 0 {
		metrics.record_reconcile_sweep(u64::try_from(sweep.aspects_squashed).unwrap_or(u64::MAX), u64::try_from(sweep.segments_removed).unwrap_or(u64::MAX));
	}
	report_sweep_failures(&span, metrics, "compact", &sweep.failed);
	report_busy_aspects(&span, "compact", &sweep.busy);
	Ok(sweep)
}

/// Spawn the background reconcile daemon on `config.interval`.
///
/// Returns the task handle; dropping it leaves the daemon running detached for the
/// process lifetime, which is the intended deployment shape.
///
/// The first sweep fires one interval after start (the immediate `interval` tick is
/// consumed first), so start-up is not stampeded by an eager pass. Each sweep that
/// reconciles at least one aspect logs a one-line summary. An aspect whose pass fails is
/// logged by the tick (one `WARN` line per aspect) while the sweep carries on with the
/// remaining aspects; a whole-sweep failure (the aspect list itself unreadable) is
/// logged and the loop continues (a transient control-plane error must not kill the
/// daemon).
#[must_use]
pub fn spawn_reconcile_daemon(store: Arc<SegmentStore>, metrics: SharedMetrics, config: ReconcileDaemonConfig) -> tokio::task::JoinHandle<()> {
	tokio::spawn(async move {
		let mut ticker = tokio::time::interval(config.interval);
		// Consume the immediate first tick so the first sweep waits one interval.
		ticker.tick().await;
		loop {
			ticker.tick().await;
			if config.hot_cold {
				match reconcile_tick_hot_cold(&store, &metrics, config.threshold).await {
					Ok(sweep) if sweep.aspects_reconciled > 0 => {
						println!("reconcile daemon (hot/cold): scanned {} aspect(s), reconciled {} aspect(s) / {} cold + {} hot segment(s)", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.cold_reconciled, sweep.hot_reconciled);
					}
					Ok(_) => {}
					Err(err) => eprintln!("reconcile daemon: hot/cold sweep failed: {err}"),
				}
			} else {
				match reconcile_tick(&store, &metrics, config.threshold).await {
					Ok(sweep) if sweep.aspects_reconciled > 0 => {
						println!("reconcile daemon: scanned {} aspect(s), reconciled {} aspect(s) / {} segment(s)", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_reconciled);
					}
					Ok(_) => {}
					Err(err) => eprintln!("reconcile daemon: sweep failed: {err}"),
				}
			}
			// Optionally also merge cross-segment overlaps this tick (an independent axis).
			// With a split floor configured, split dominant cold prefixes rather than
			// fully rewriting each component.
			if config.overlaps {
				let tick = match config.split_min_bytes {
					Some(min) => reconcile_tick_overlaps_with_policy(&store, &metrics, min).await,
					None => reconcile_tick_overlaps(&store, &metrics).await,
				};
				match tick {
					Ok(sweep) if sweep.aspects_reconciled > 0 => {
						println!("reconcile daemon (overlaps): scanned {} aspect(s), merged {} aspect(s) / removed {} segment(s)", sweep.aspects_scanned, sweep.aspects_reconciled, sweep.segments_removed);
					}
					Ok(_) => {}
					Err(err) => eprintln!("reconcile daemon: overlap sweep failed: {err}"),
				}
			}
			// Optionally cap split-path fragmentation by squashing over-threshold aspects
			// (runs after the overlap merge, which is what accumulates the split segments).
			if let Some(max_segments) = config.squash_max_segments {
				match reconcile_tick_squash(&store, &metrics, max_segments).await {
					Ok(sweep) if sweep.aspects_squashed > 0 => {
						println!("reconcile daemon (squash): scanned {} aspect(s), squashed {} aspect(s) / removed {} segment(s)", sweep.aspects_scanned, sweep.aspects_squashed, sweep.segments_removed);
					}
					Ok(_) => {}
					Err(err) => eprintln!("reconcile daemon: squash sweep failed: {err}"),
				}
			}
			// Optionally coalesce toward a target segment size (size-aware fragmentation
			// bound; runs after the squash-to-one pass, an independent axis).
			if let Some(target_rows) = config.compact_target_rows {
				match reconcile_tick_compact(&store, &metrics, target_rows).await {
					Ok(sweep) if sweep.aspects_squashed > 0 => {
						println!("reconcile daemon (compact): scanned {} aspect(s), compacted {} aspect(s) / removed {} segment(s)", sweep.aspects_scanned, sweep.aspects_squashed, sweep.segments_removed);
					}
					Ok(_) => {}
					Err(err) => eprintln!("reconcile daemon: compaction sweep failed: {err}"),
				}
			}
		}
	})
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use bigdecimal::BigDecimal;
	use tempfile::TempDir;
	use weft_physical_type::{AspectSchema, PhysicalType, TimeUnit};
	use weftdb::SegmentStore;

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

	#[tokio::test]
	async fn hot_cold_tick_reconciles_cold_segments_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		// aspect a: a cold out-of-order segment (id 0) plus a hot-tail out-of-order
		// segment (id 1) — backlog 2.
		store.seal("a", &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.unwrap();
		store.seal("a", &schema(), &[200_i64, 240, 210], &[bd("4"), bd("6"), bd("5")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		// Threshold 3 is above the backlog: the cold segment reconciles, the hot tail
		// is deferred, and the one reconciled aspect / one segment are recorded.
		let sweep = reconcile_tick_hot_cold(&store, &metrics, 3).await.unwrap();
		let snap = metrics.snapshot();
		assert_eq!(sweep.aspects_reconciled, 1);
		assert_eq!(sweep.cold_reconciled, 1, "the cold segment is reconciled below threshold");
		assert_eq!(sweep.hot_reconciled, 0, "the hot tail is deferred below threshold");
		assert_eq!(sweep.segments_reconciled(), 1);
		assert_eq!(snap.reconcile.passes, 1, "one aspect reconciled = one pass");
		assert_eq!(snap.reconcile.segments_reconciled, 1);
		assert_eq!(store.aspect_stats("a").await.unwrap().unsorted_segments, 1, "hot tail still out of order");

		// A second tick at threshold 3 now finds only the deferred hot tail (backlog 1
		// < 3) — nothing is rewritten and the counters do not move.
		let quiet = reconcile_tick_hot_cold(&store, &metrics, 3).await.unwrap();
		let snap2 = metrics.snapshot();
		drop(store);
		assert_eq!(quiet.aspects_reconciled, 0);
		assert_eq!(snap2.reconcile.passes, 1, "the deferred hot tail did not bump the pass counter");
	}

	#[tokio::test]
	async fn overlaps_tick_merges_overlapping_segments_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		// aspect a: two internally-sorted but time-overlapping segments.
		store.seal("a", &schema(), &[0_i64, 10, 20], &[bd("1"), bd("2"), bd("3")]).await.unwrap();
		store.seal("a", &schema(), &[10_i64, 20, 30], &[bd("4"), bd("5"), bd("6")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		let sweep = reconcile_tick_overlaps(&store, &metrics).await.unwrap();
		let snap = metrics.snapshot();
		assert_eq!(sweep.aspects_reconciled, 1);
		assert_eq!(sweep.segments_removed, 1, "the overlapping pair merged to one");
		assert_eq!(snap.reconcile.passes, 1, "one aspect merged = one pass");
		assert_eq!(snap.reconcile.segments_reconciled, 1);
		assert_eq!(store.aspect_stats("a").await.unwrap().overlapping_segments, 0);

		// A second tick has nothing to merge and records nothing.
		let quiet = reconcile_tick_overlaps(&store, &metrics).await.unwrap();
		let snap2 = metrics.snapshot();
		drop(store);
		assert_eq!(quiet.aspects_reconciled, 0);
		assert_eq!(snap2.reconcile.passes, 1, "the quiet overlap tick did not bump the pass counter");
	}

	#[tokio::test]
	async fn overlaps_tick_with_policy_splits_a_dominant_cold_prefix() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		// A long cold base [0..100] + a small late tail re-entering only its end.
		let base_ts: Vec<i64> = (0..=10).map(|i| i * 10).collect();
		let base_vs: Vec<BigDecimal> = (0..=10).map(BigDecimal::from).collect();
		store.seal("a", &schema(), &base_ts, &base_vs).await.unwrap();
		store.seal("a", &schema(), &[90_i64, 100, 110], &[bd("900"), bd("1000"), bd("1100")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		// A tiny floor splits the cold prefix off (2 segments) instead of one full rewrite.
		let sweep = reconcile_tick_overlaps_with_policy(&store, &metrics, 1).await.unwrap();
		let snap = metrics.snapshot();
		let stats = store.aspect_stats("a").await.unwrap();
		drop(store);
		assert_eq!(sweep.aspects_reconciled, 1, "the overlapping aspect was reconciled");
		assert_eq!(sweep.segments_removed, 0, "a two-member split removes no segment net");
		assert_eq!(snap.reconcile.passes, 1, "a split is still a pass");
		assert_eq!(stats.segment_count, 2, "cold prefix split off from the hot suffix");
		assert_eq!(stats.overlapping_segments, 0);
	}

	#[tokio::test]
	async fn squash_tick_folds_over_threshold_aspects_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		// Three disjoint segments (over a cap of 2).
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.unwrap();
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.unwrap();
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		let sweep = reconcile_tick_squash(&store, &metrics, 2).await.unwrap();
		let snap = metrics.snapshot();
		let count = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(sweep.aspects_squashed, 1);
		assert_eq!(sweep.segments_removed, 2, "3 segments squashed to 1");
		assert_eq!(snap.reconcile.passes, 1, "a squash is a pass");
		assert_eq!(count, 1);

		// A second tick at the same cap finds one segment (<= 2) and records nothing.
		let store = SegmentStore::open(dir.path()).await.unwrap();
		let quiet = reconcile_tick_squash(&store, &metrics, 2).await.unwrap();
		let snap2 = metrics.snapshot();
		drop(store);
		assert_eq!(quiet.aspects_squashed, 0);
		assert_eq!(snap2.reconcile.passes, 1, "the quiet squash tick did not bump the pass counter");
	}

	#[tokio::test]
	async fn compact_tick_coalesces_toward_target_and_records_metrics() {
		let dir = TempDir::new().unwrap();
		let store = SegmentStore::open(dir.path()).await.unwrap();
		store.declare("a", &schema()).await.unwrap();
		// Four disjoint 2-row segments; target 4 coalesces them in pairs → two segments
		// (NOT one, unlike the squash tick), holding fragmentation near the target size.
		store.seal("a", &schema(), &[0_i64, 10], &[bd("0"), bd("1")]).await.unwrap();
		store.seal("a", &schema(), &[20_i64, 30], &[bd("2"), bd("3")]).await.unwrap();
		store.seal("a", &schema(), &[40_i64, 50], &[bd("4"), bd("5")]).await.unwrap();
		store.seal("a", &schema(), &[60_i64, 70], &[bd("6"), bd("7")]).await.unwrap();
		let metrics: SharedMetrics = Arc::new(Metrics::default());

		let sweep = reconcile_tick_compact(&store, &metrics, 4).await.unwrap();
		let snap = metrics.snapshot();
		let count = store.segment_count("a").await.unwrap();
		drop(store);
		assert_eq!(sweep.aspects_squashed, 1);
		assert_eq!(sweep.segments_removed, 2, "four segments coalesced to two (two pairs)");
		assert_eq!(snap.reconcile.passes, 1, "a compaction is a pass");
		assert_eq!(count, 2, "held near the target size, not folded to one");

		// A second tick: the two segments are each already at the 4-row target → no-op.
		let store = SegmentStore::open(dir.path()).await.unwrap();
		let quiet = reconcile_tick_compact(&store, &metrics, 4).await.unwrap();
		let snap2 = metrics.snapshot();
		drop(store);
		assert_eq!(quiet.aspects_squashed, 0);
		assert_eq!(snap2.reconcile.passes, 1, "the quiet compaction tick did not bump the pass counter");
	}

	/// A `tracing` writer that appends every formatted line to a shared buffer, so a test
	/// can count exactly what a tick logged.
	#[derive(Clone, Default)]
	struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

	impl std::io::Write for CapturedLog {
		fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
			self.0.lock().expect("log buffer").extend_from_slice(buf);
			Ok(buf.len())
		}

		fn flush(&mut self) -> std::io::Result<()> {
			Ok(())
		}
	}

	impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
		type Writer = Self;

		fn make_writer(&'a self) -> Self::Writer {
			self.clone()
		}
	}

	impl CapturedLog {
		/// A `fmt` subscriber at `INFO` (the daemon's default filter) writing into this log.
		fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync {
			tracing_subscriber::fmt().with_writer(self.clone()).with_ansi(false).with_max_level(tracing::Level::INFO).finish()
		}

		/// The captured lines logged at `WARN` or `ERROR`.
		fn failure_lines(&self) -> Vec<String> {
			let bytes = self.0.lock().expect("log buffer").clone();
			String::from_utf8(bytes).expect("utf-8 log").lines().filter(|line| line.contains("WARN") || line.contains("ERROR")).map(str::to_owned).collect()
		}
	}

	/// The crash-consistency S4 fixture: six aspects (`s1`..`s6`, name order), each with
	/// three internally out-of-order, transitively time-overlapping 3-row segments so every
	/// tick kind has work on every aspect, and `s2`'s lowest-id frame cut to half its length
	/// so any pass that decodes it fails on that aspect.
	async fn sweep_isolation_fixture(dir: &std::path::Path) -> SegmentStore {
		let store = SegmentStore::open(dir).await.unwrap();
		for aspect in ["s1", "s2", "s3", "s4", "s5", "s6"] {
			store.declare(aspect, &schema()).await.unwrap();
			let first = store.seal(aspect, &schema(), &[100_i64, 130, 110], &[bd("1"), bd("3"), bd("2")]).await.unwrap();
			store.seal(aspect, &schema(), &[120_i64, 150, 125], &[bd("4"), bd("6"), bd("5")]).await.unwrap();
			store.seal(aspect, &schema(), &[140_i64, 170, 145], &[bd("7"), bd("9"), bd("8")]).await.unwrap();
			if aspect == "s2" {
				let file = std::fs::OpenOptions::new().write(true).open(&first.path).unwrap();
				let len = file.metadata().unwrap().len();
				file.set_len(len / 2).unwrap();
			}
		}
		store
	}

	/// Crash-consistency S4: a tick over a store with one truncated frame logs exactly one
	/// failure line (naming the bad aspect), counts it once in
	/// `weft_reconcile_failed_passes_total`, and still reconciles the five healthy aspects.
	#[tokio::test]
	async fn tick_logs_one_line_per_failed_aspect_and_maintains_the_rest() {
		let dir = TempDir::new().unwrap();
		let store = sweep_isolation_fixture(dir.path()).await;
		let metrics: SharedMetrics = Arc::new(Metrics::default());
		let log = CapturedLog::default();
		let guard = tracing::subscriber::set_default(log.subscriber());
		let sweep = reconcile_tick(&store, &metrics, 1).await.expect("a bad aspect does not fail the tick");
		drop(guard);
		let s6_unsorted = store.aspect_stats("s6").await.unwrap().unsorted_segments;
		drop(store);
		let lines = log.failure_lines();
		let snap = metrics.snapshot();
		assert_eq!(lines.len(), 1, "exactly one failure line: {lines:#?}");
		assert!(lines[0].contains("aspect=s2"), "the line names the bad aspect: {}", lines[0]);
		assert!(lines[0].contains("sweep=\"threshold\""), "the line names the sweep kind: {}", lines[0]);
		assert!(lines[0].contains("s2-0.weftseg"), "the line carries the error chain down to the frame: {}", lines[0]);
		assert_eq!(sweep.aspects_reconciled, 5, "every healthy aspect is reconciled");
		assert_eq!(sweep.failed.len(), 1);
		assert_eq!(s6_unsorted, 0, "the last aspect in name order is still reached");
		assert_eq!(snap.reconcile.failed_passes, 1, "one failed aspect pass counted");
		assert_eq!(snap.reconcile.passes, 5, "the healthy aspects still count as passes");
	}

	/// Crash-consistency S4: every tick kind the daemon runs isolates the bad aspect the
	/// same way: one failure line, one counted failed pass, five healthy aspects maintained.
	#[tokio::test]
	async fn every_tick_kind_isolates_a_truncated_frame() {
		for kind in ["threshold", "hot_cold", "overlaps", "overlaps_split", "squash", "compact"] {
			let dir = TempDir::new().unwrap();
			let store = sweep_isolation_fixture(dir.path()).await;
			let metrics: SharedMetrics = Arc::new(Metrics::default());
			let log = CapturedLog::default();
			let guard = tracing::subscriber::set_default(log.subscriber());
			let (maintained, failed) = match kind {
				"threshold" => reconcile_tick(&store, &metrics, 1).await.map(|s| (s.aspects_reconciled, s.failed.len())),
				"hot_cold" => reconcile_tick_hot_cold(&store, &metrics, 1).await.map(|s| (s.aspects_reconciled, s.failed.len())),
				"overlaps" => reconcile_tick_overlaps(&store, &metrics).await.map(|s| (s.aspects_reconciled, s.failed.len())),
				"overlaps_split" => reconcile_tick_overlaps_with_policy(&store, &metrics, 1).await.map(|s| (s.aspects_reconciled, s.failed.len())),
				"squash" => reconcile_tick_squash(&store, &metrics, 2).await.map(|s| (s.aspects_squashed, s.failed.len())),
				"compact" => reconcile_tick_compact(&store, &metrics, 6).await.map(|s| (s.aspects_squashed, s.failed.len())),
				_ => unreachable!("unknown tick kind {kind}"),
			}
			.unwrap_or_else(|err| panic!("{kind}: a bad aspect failed the whole tick: {err:#}"));
			drop(guard);
			drop(store);
			let lines = log.failure_lines();
			assert_eq!(lines.len(), 1, "{kind}: exactly one failure line: {lines:#?}");
			assert!(lines[0].contains("aspect=s2"), "{kind}: the line names the bad aspect: {}", lines[0]);
			assert_eq!(maintained, 5, "{kind}: every healthy aspect is maintained");
			assert_eq!(failed, 1, "{kind}: only the bad aspect fails");
			assert_eq!(metrics.snapshot().reconcile.failed_passes, 1, "{kind}: one failed aspect pass counted");
		}
	}
}
