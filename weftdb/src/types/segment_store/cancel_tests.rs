//! ROB-21a: cooperative between-aspect maintenance cancellation.
//!
//! Tests A1–A3 from the brief: sweep cancellation is coarse (between aspects only),
//! never interrupts an in-flight aspect, and the RAII cleanup chain
//! (`PendingOutputs` → `Reaper::abandon`) remains intact after a cancelled sweep.
//!
//! ROB-21 remainder: A4 drops a sweep future mid-swap — the abort the daemon
//! supervisor performs on shutdown — and pins that the same RAII chain covers
//! that path too (see `abort_child`, which runs in a child process).

use std::{path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{MaintenanceWait, SegmentStore};
use crate::{
	durable::fault::{self, FaultPoint}, IngestBatch, MaintenanceCancel
};

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

// ---------------------------------------------------------------------------
// A4: Drop a sweep future mid-swap — the daemon supervisor's shutdown abort.
// ---------------------------------------------------------------------------

/// Set only in the child process
/// [`a_dropped_sweep_releases_its_aspect_and_settles_its_outputs`] starts: the
/// root to open.
const ABORT_CHILD_ROOT: &str = "WEFT_TEST_ROB21_ABORT_CHILD_ROOT";

/// The `.weftseg` files directly under `root/segments`, by name.
fn frames_in(root: &std::path::Path) -> Vec<String> {
	let mut names: Vec<String> = std::fs::read_dir(root.join("segments")).expect("lists segments/").map(|entry| entry.expect("an entry").file_name().to_string_lossy().into_owned()).filter(|name| name.ends_with(".weftseg")).collect();
	names.sort();
	names
}

/// The file names `aspect`'s live rows reference, by name.
async fn referenced_frames(store: &SegmentStore, aspect: &str) -> Vec<String> {
	let mut names: Vec<String> = store.index().all(aspect).await.expect("reads the index").iter().map(|descriptor| descriptor.path.rsplit(['/', '\\']).next().expect("a file name").to_string()).collect();
	names.sort();
	names
}

/// The body the child runs, in a process of its own because it arms
/// `M-pending-committed` in-process with a pause (a point every maintenance swap
/// passes).
///
/// A store-wide squash sweep parked mid-swap on its first aspect, then aborted
/// the way the daemon supervisor stops a daemon on shutdown (its task dropped),
/// must leave nothing held: the aspect's maintenance lock is free, the output
/// the parked swap had journaled is handed to the reaper, and the next sweep
/// converges the store — the RAII cleanup chain (`MaintGuard`,
/// `PendingOutputs` → `Reaper::abandon`) covering the abort path, not just the
/// cooperative cancellation A1–A3 tested. The per-aspect half of this chain is
/// already pinned by the dropped-swap test; this one pins the sweep level, where
/// the lock and the abandoned output belong to a sweep that never returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abort_child() {
	let Some(root) = std::env::var_os(ABORT_CHILD_ROOT) else { return };
	let root = PathBuf::from(root);
	let store = Arc::new(store_with(root.as_path(), &["alpha", "beta"]).await);
	seal_many(&store, "alpha", 5, 10).await;
	seal_many(&store, "beta", 5, 10).await;

	// Park the sweep inside alpha's squash, after its output's pending row
	// committed and before the output was written: the guard holds the
	// journaled name. (`M-pending-committed` is an async hit, so aborting the
	// sweep parks no blocking thread behind it.)
	let resume = Arc::new(tokio::sync::Notify::new());
	let before = fault::hits(FaultPoint::MPendingCommitted);
	let armed = fault::arm(FaultPoint::MPendingCommitted, fault::FaultAction::Pause(resume.clone()));
	let sweep = tokio::spawn({
		let store = store.clone();
		async move { store.squash_all_over_threshold(2, MaintenanceWait::Skip).await }
	});
	tokio::time::timeout(Duration::from_secs(30), fault::reached(FaultPoint::MPendingCommitted, before + 1)).await.expect("the sweep reaches M-pending-committed");
	// The daemon supervisor's shutdown: drop the sweep future mid-swap.
	sweep.abort();
	let aborted = sweep.await.expect_err("the aborted sweep joins");
	assert!(aborted.is_cancelled(), "the sweep task was aborted, not failed: {aborted:?}");
	// The dropped guard handed the journaled output to the reaper on the spot.
	assert_eq!(store.reaper.abandoned("alpha").len(), 1, "the guard handed its one output to the reaper");
	drop(armed); // Disarm before the next sweep, which must not park.

	// The lock is free: a Skip sweep takes alpha at once instead of listing it
	// busy, and both aspects converge.
	let rest = store.squash_all_over_threshold(2, MaintenanceWait::Skip).await.expect("the next sweep runs");
	assert!(rest.busy.is_empty(), "the aborted sweep released alpha's lock: {:?}", rest.busy);
	assert!(rest.failed.is_empty(), "nothing failed: {:?}", rest.failed);
	assert_eq!(rest.aspects_squashed, 2, "both aspects converge after the abort");

	// The parked swap's output settles: a reap finds nothing left waiting and
	// the abandoned set is empty, and a reopen — whose journal replay is the
	// backstop — reads every row back with only live frames on disk.
	let reaped = store.reap(MaintenanceWait::Skip).await.expect("reaps");
	assert_eq!(reaped.frames_waiting, 0, "no retired frame is left waiting");
	assert!(reaped.failed.is_empty(), "nothing failed to reap: {:?}", reaped.failed);
	assert!(store.reaper.abandoned("alpha").is_empty(), "the abandoned output was settled");
	drop(store);
	let store = SegmentStore::open(&root).await.expect("reopens");
	for aspect in ["alpha", "beta"] {
		assert_eq!(store.segment_count(aspect).await.expect("counts"), 1, "{aspect} converged to one segment");
		let (ts, vs) = store.read_time_range(aspect, i64::MIN, i64::MAX).await.expect("reads");
		let expected_ts: Vec<i64> = (1..=5).flat_map(|tag: i64| (0..10).map(move |i| tag * 1000 + i * 10)).collect();
		assert_eq!(ts, expected_ts, "{aspect} kept every timestamp");
		assert_eq!(vs, expected_ts.iter().map(|t| Some(BigDecimal::from(*t))).collect::<Vec<_>>(), "{aspect} kept every value");
	}
	let frames = frames_in(&root);
	let mut referenced = Vec::new();
	for aspect in ["alpha", "beta"] {
		referenced.extend(referenced_frames(&store, aspect).await);
	}
	referenced.sort();
	drop(store);
	assert_eq!(frames, referenced, "no frame is left but the live rows'");
}

/// ROB-21: a store-wide sweep dropped mid-swap (the daemon supervisor's shutdown
/// path) releases its aspect and settles its outputs — see [`abort_child`],
/// which runs in a child process because it arms `M-pending-committed` in-process.
#[tokio::test]
async fn a_dropped_sweep_releases_its_aspect_and_settles_its_outputs() {
	let dir = TempDir::new().expect("tempdir");
	let exe = std::env::current_exe().expect("finds the test binary");
	let out = tokio::process::Command::new(exe).args(["types::segment_store::cancel_tests::abort_child", "--exact", "--nocapture", "--test-threads=1"]).env(ABORT_CHILD_ROOT, dir.path()).output().await.expect("runs the child");
	assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"), "the aborted sweep left the store converged, with no held lock and no stray frame: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}
