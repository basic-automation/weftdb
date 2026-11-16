//! ROB-20 cut 1: a maintenance pass that would fold more than the store's per-operation
//! output budget is refused **before the group is read**, which is the memory spike the
//! budget exists to bound. The budget rides [`SegmentStoreOptions`], so a test sets it at
//! open time and watches the refusal on a real filesystem, with no fake — the same shape
//! as [`disk_guard_tests`](super::disk_guard_tests).
//!
//! The case this file exists to pin is the second one: a refusal is not a lost merge. The
//! aspect keeps every segment it had, and the same pass succeeds once the budget is raised.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{maintenance_output_budget_from, SegmentStore, SegmentStoreOptions};
use crate::{IngestBatch, StoreError, UpsertKeys};

/// The aspect every test here writes.
const ASPECT: &str = "price";

fn values(from: i64, count: i64) -> (Vec<i64>, Vec<BigDecimal>) {
	(from..from + count).map(|t| (t, BigDecimal::from_str(&t.to_string()).expect("parses"))).unzip()
}

/// A fresh store at `root` with [`ASPECT`] declared (lossless f64, seconds) and upsert keys
/// declared, as a consumer would for data that arrives as updates: an overlap within a key
/// is the store's newer-wins rather than duplicate rows, which is what lets two overlapping
/// segments exist at all. The pattern is the one [`overlap_read_tests`](super::overlap_read_tests)
/// uses. Declaring after the open is what lets a reopened root carry the aspect.
async fn store_at(root: &Path, budget: Option<u64>) -> SegmentStore {
	let mut options = SegmentStoreOptions::default();
	if let Some(budget) = budget {
		options = options.with_max_maintenance_output_bytes(budget);
	}
	let store = SegmentStore::open_with_options(root, options).await.expect("opens");
	let keys = UpsertKeys::new(["host"]).expect("valid keys");
	store.declare_with_upsert_keys(ASPECT, &AspectSchema::new(PhysicalType::F64, BigDecimal::from(0), TimeUnit::Seconds), Some(&keys)).await.expect("declares");
	store
}

/// Everything under `segments/`, sorted by name: a refusal must leave this list exactly as
/// it was, including no half-written frame and no staging directory. Sorted because
/// `read_dir` gives no order of its own.
fn segment_entries(root: &Path) -> Vec<String> {
	let mut names = std::fs::read_dir(root.join("segments")).map(|dir| dir.filter_map(Result::ok).filter_map(|entry| entry.file_name().to_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default();
	names.sort();
	names
}

/// Ingest two dense batches that genuinely overlap — 0..=999 and 500..=1499, with different
/// values at the shared instants — so the reconcile pass has a component of two members to
/// fold. This store has no open memtable: each ingest seals its own frame and records its
/// own index row, so two ingests are two segments.
///
/// The two batches carry **different ingest keys**, which is not cosmetic: the ingest ledger
/// records `(aspect, key)` with a fingerprint of the batch, and offering the same key with a
/// different fingerprint is refused as `StoreError::IdempotencyConflict` before any frame is
/// written (the ledger check inside `ingest`, `weftdb/src/types/segment_store/seal.rs`).
/// Reusing one key here would test the ledger, not the budget, and the second
/// write would never reach the segment tree.
async fn seal_two_overlapping(store: &SegmentStore) {
	let (ts, vs) = values(0, 1_000);
	store.ingest(ASPECT, &IngestBatch::dense("budget-fixture-first", ts, vs)).await.expect("first ingest");
	let (ts, vs) = values(500, 1_000);
	store.ingest(ASPECT, &IngestBatch::dense("budget-fixture-second", ts, vs)).await.expect("second ingest");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 2, "the fixture really does hold two overlapping segments");
}

#[test]
fn the_library_default_leaves_the_pass_unbudgeted() {
	assert_eq!(SegmentStoreOptions::default().max_maintenance_output_bytes, None, "a library open budgets nothing, so it refuses no maintenance it did not refuse before");
	assert_eq!(SegmentStoreOptions::default().with_max_maintenance_output_bytes(7).max_maintenance_output_bytes, Some(7));
}

#[test]
fn the_knob_is_a_positive_byte_count_and_nothing_else() {
	for (raw, want) in [
		(None, None),
		(Some(""), None),
		(Some("   "), None),
		(Some("forty"), None),
		(Some("-1"), None),
		(Some("1.5"), None),
		// Zero means unbudgeted, not "no maintenance at all": a mistyped zero must not
		// stop the sweeper, which is the same leniency the disk guard's reserve uses.
		(Some("0"), None),
		(Some("4096"), Some(4096)),
		(Some(" 4096 "), Some(4096)),
		(Some("18446744073709551615"), Some(u64::MAX)),
	] {
		assert_eq!(maintenance_output_budget_from(raw), want, "{raw:?} parses as {want:?}");
	}
}

#[tokio::test]
async fn a_group_over_the_budget_is_refused_before_anything_is_read() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), Some(1)).await;
	seal_two_overlapping(&store).await;
	let before = segment_entries(dir.path());

	let err = store.reconcile_overlaps(ASPECT).await.expect_err("a one-byte budget cannot admit a real merge");
	let refusal = err.downcast_ref::<StoreError>().expect("a typed refusal through the pass's context");
	let StoreError::OverBudget { aspect, budget_bytes, planned_bytes } = refusal else {
		panic!("expected OverBudget, got {refusal:?}");
	};
	assert_eq!(aspect, ASPECT);
	assert_eq!(*budget_bytes, 1);
	assert!(*planned_bytes > 1, "the plan named the bytes it wanted to fold, and it was over the budget");

	// The point of refusing at the plan rather than at the swap: nothing was written, no
	// member was marked, and no staging tree was opened.
	assert_eq!(segment_entries(dir.path()), before, "the refusal left the segment tree exactly as it was");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 2, "the aspect keeps both segments");
	drop(store);
}

#[tokio::test]
async fn a_refusal_is_not_a_lost_merge_the_same_pass_runs_once_the_budget_rises() {
	let dir = TempDir::new().unwrap();
	{
		let store = store_at(dir.path(), Some(1)).await;
		seal_two_overlapping(&store).await;
		assert!(store.reconcile_overlaps(ASPECT).await.is_err(), "refused under the impossible budget");
		drop(store);
	}
	// The same store, the same two segments, a budget that admits them.
	let store = store_at(dir.path(), Some(u64::MAX)).await;
	let merged = store.reconcile_overlaps(ASPECT).await.expect("admitted under a satisfiable budget");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 1, "the fold left one segment");
	drop(store);
}

#[tokio::test]
async fn an_unbudgeted_store_runs_the_same_pass_it_ran_before() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), None).await;
	seal_two_overlapping(&store).await;
	let merged = store.reconcile_overlaps(ASPECT).await.expect("no budget, no refusal");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 1);
	drop(store);
}

#[tokio::test]
async fn a_group_that_fits_the_budget_is_never_considered_a_refusal() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), Some(1 << 40)).await;
	seal_two_overlapping(&store).await;
	let before = segment_entries(dir.path());
	// A budget far above the group's bytes is indistinguishable from no budget at all: it
	// must not change what the pass writes, only allow what it already allowed.
	let merged = store.reconcile_overlaps(ASPECT).await.expect("the group is far under the budget");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_ne!(segment_entries(dir.path()), before, "the fold did write, so the pass is not silently skipped");
	drop(store);
}
