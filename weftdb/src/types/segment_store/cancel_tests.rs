//! ROB-21a: cooperative between-aspect maintenance cancellation.
//!
//! Tests A1–A3 from the brief: sweep cancellation is coarse (between aspects only),
//! never interrupts an in-flight aspect, and the RAII cleanup chain
//! (`PendingOutputs` → `Reaper::abandon`) remains intact after a cancelled sweep.

use std::str::FromStr;

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{MaintenanceWait, SegmentStore};
use crate::{IngestBatch, MaintenanceCancel};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// A fresh store at `root` with `aspects` declared.
async fn store_with(root: &std::path::Path, aspects: &[&str]) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	for aspect in aspects {
		store.declare(aspect, &schema()).await.expect("declares");
	}
	store
}

/// Seal a multi-row segment into `aspect`.
async fn seal_seg(aspect: &str, store: &SegmentStore, tag: i64, rows: usize) {
	let ts: Vec<i64> = (0..rows).map(|i| tag * 1000 + i64::try_from(i).expect("row index fits") * 10).collect();
	let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(*t)).collect();
	let batch = IngestBatch::dense(format!("{aspect}-{tag}"), ts, vs);
	let receipt = store.ingest(aspect, &batch).await.expect("ingests");
	assert!(!receipt.segment_ids.is_empty(), "at least one segment");
}

/// Create `count` segments in `aspect`, each with `rows` rows.
async fn seal_many(store: &SegmentStore, aspect: &str, count: usize, rows: usize) {
	for i in 0..count {
		seal_seg(aspect, store, i64::try_from(i).expect("count fits") + 1, rows).await;
	}
}

// ---------------------------------------------------------------------------
// A1: Cancel *between* aspects — the in-flight aspect runs to completion.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cancel_between_aspects_finishes_current_and_reports_cancelled() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &["alpha", "beta", "gamma", "delta"]).await;

	seal_many(&store, "alpha", 5, 10).await;
	seal_many(&store, "beta", 5, 10).await;
	seal_many(&store, "gamma", 5, 10).await;
	seal_many(&store, "delta", 5, 10).await;

	// Poll-budget token: the sweep is allowed exactly one aspect, then sees a
	// request. A wall-clock cancellor cannot pin this — four tiny aspects are
	// swept in well under a millisecond, so a 1 ms sleep either lands after the
	// last aspect (nothing to cancel) or races the loop.
	let cancel = MaintenanceCancel::after_polls(1);

	let sweep = store.squash_all_over_threshold_with_cancel(2, MaintenanceWait::Skip, &cancel).await.expect("sweep succeeds");

	assert!(sweep.cancelled, "sweep was cancelled before visiting all aspects");
	assert_eq!(sweep.aspects_squashed, 1, "exactly the in-flight aspect completed, no later aspect was started");

	// No leaked `MaintGuard` and nothing half-done: an uncancelled sweep finishes
	// the three aspects the cancelled one never reached.
	let rest = store.squash_all_over_threshold(2, MaintenanceWait::Skip).await.expect("second sweep succeeds");
	assert_eq!(rest.aspects_squashed, 3, "the remaining aspects were still schedulable and got swept");
	assert!(!rest.cancelled, "an uncancelled sweep does not report cancellation");
	drop(store);
}

// ---------------------------------------------------------------------------
// A2: Pre-requested token — zero aspects scanned, cancelled = true.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cancel_before_sweep_starts_nothing() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &["alpha", "beta"]).await;

	seal_many(&store, "alpha", 3, 1).await;
	seal_many(&store, "beta", 3, 1).await;

	let cancel = MaintenanceCancel::default();
	cancel.request(); // Pre-requested.

	let sweep = store.squash_all_over_threshold_with_cancel(2, MaintenanceWait::Skip, &cancel).await.expect("sweep succeeds");

	assert!(sweep.cancelled, "sweep should report cancelled");
	assert_eq!(sweep.aspects_squashed, 0, "no aspect should have been squashed");

	// Both aspects untouched.
	for aspect in &["alpha", "beta"] {
		let count = store.segment_count(aspect).await.expect("count");
		assert_eq!(count, 3, "{aspect} should still have 3 segments");
	}

	drop(store);
}

// ---------------------------------------------------------------------------
// A3: Cancelled sweep then full sweep converges — RAII pinning evidence.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn cancelled_sweep_then_full_sweep_converges() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &["alpha", "beta"]).await;

	// Both aspects have work (4 segments each, threshold 2).
	seal_many(&store, "alpha", 4, 1).await;
	seal_many(&store, "beta", 4, 1).await;

	// Step 1 — cancelled sweep.
	let cancel = MaintenanceCancel::default();
	cancel.request();
	let c1 = store.squash_all_over_threshold_with_cancel(2, MaintenanceWait::Skip, &cancel).await.expect("cancelled sweep");
	assert!(c1.cancelled, "first sweep reports cancelled");
	assert_eq!(c1.aspects_squashed, 0, "nothing squashed on cancelled sweep");

	// Step 2 — full uncancelled sweep.
	let c2 = store.squash_all_over_threshold(2, MaintenanceWait::Skip).await.expect("full sweep");
	assert!(!c2.cancelled, "full sweep not cancelled");
	assert_eq!(c2.aspects_squashed, 2, "both aspects should be squashed");
	assert!(c2.segments_removed > 0, "segments were removed");

	// Both aspects converged: segment count <= 2 (threshold).
	for aspect in &["alpha", "beta"] {
		let count = store.segment_count(aspect).await.expect("count");
		assert!(count <= 2, "{aspect} should be at or below threshold; has {count}");
	}

	drop(store);
}
