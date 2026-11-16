//! ROB-20's two bounds on what a fold may do: a pass that would fold more than the store's
//! per-operation output budget is refused **before the group is read** (the memory spike the
//! budget exists to bound), and a fold that would leave a frame above the store's segment
//! ceiling has that output **re-cut** instead. The budget rides [`SegmentStoreOptions`], so a test sets it at
//! open time and watches the refusal on a real filesystem, with no fake — the same shape
//! as [`disk_guard_tests`](super::disk_guard_tests).
//!
//! The case this file exists to pin is the second one: a refusal is not a lost merge. The
//! aspect keeps every segment it had, and the same pass succeeds once the budget is raised.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::{maint::size_driven_cuts, maintenance_output_budget_from, maintenance_segment_ceiling_from, SegmentStore, SegmentStoreOptions};
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
async fn store_at(root: &Path, configure: impl FnOnce(SegmentStoreOptions) -> SegmentStoreOptions) -> SegmentStore {
	let options = configure(SegmentStoreOptions::default());
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
	let store = store_at(dir.path(), |options| options.with_max_maintenance_output_bytes(1)).await;
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
		let store = store_at(dir.path(), |options| options.with_max_maintenance_output_bytes(1)).await;
		seal_two_overlapping(&store).await;
		assert!(store.reconcile_overlaps(ASPECT).await.is_err(), "refused under the impossible budget");
		drop(store);
	}
	// The same store, the same two segments, a budget that admits them.
	let store = store_at(dir.path(), |options| options.with_max_maintenance_output_bytes(u64::MAX)).await;
	let merged = store.reconcile_overlaps(ASPECT).await.expect("admitted under a satisfiable budget");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 1, "the fold left one segment");
	drop(store);
}

#[tokio::test]
async fn an_unbudgeted_store_runs_the_same_pass_it_ran_before() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), |options| options).await;
	seal_two_overlapping(&store).await;
	let merged = store.reconcile_overlaps(ASPECT).await.expect("no budget, no refusal");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_eq!(store.segment_count(ASPECT).await.expect("counted"), 1);
	drop(store);
}

#[tokio::test]
async fn a_group_that_fits_the_budget_is_never_considered_a_refusal() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), |options| options.with_max_maintenance_output_bytes(1 << 40)).await;
	seal_two_overlapping(&store).await;
	let before = segment_entries(dir.path());
	// A budget far above the group's bytes is indistinguishable from no budget at all: it
	// must not change what the pass writes, only allow what it already allowed.
	let merged = store.reconcile_overlaps(ASPECT).await.expect("the group is far under the budget");
	assert!(merged >= 1, "the pass reported the component it folded, reported {merged}");
	assert_ne!(segment_entries(dir.path()), before, "the fold did write, so the pass is not silently skipped");
	drop(store);
}

/// The largest frame a fold may leave, `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` (G4). A fold's
/// output is the sum of its inputs' realized bytes, which over-counts, so a ceiling that small
/// is always exceeded and the division always reaches its cap.
fn ceiling(bytes: u64) -> impl FnOnce(SegmentStoreOptions) -> SegmentStoreOptions {
	move |options| options.with_max_maintenance_segment_bytes(bytes)
}

#[test]
fn the_library_default_re_splits_nothing() {
	assert_eq!(SegmentStoreOptions::default().max_maintenance_segment_bytes, None, "a library open re-splits no frame it did not re-split before");
	assert_eq!(SegmentStoreOptions::default().with_max_maintenance_segment_bytes(9).max_maintenance_segment_bytes, Some(9));
}

#[test]
fn the_ceiling_is_a_positive_byte_count_and_nothing_else() {
	for (raw, want) in [
		(None, None),
		(Some(""), None),
		(Some("   "), None),
		(Some("twenty"), None),
		(Some("-1"), None),
		(Some("2.5"), None),
		// Zero means the ceiling is off, not "no frame may be even one byte": a mistyped zero
		// must not turn every fold into a re-split, the same leniency as the output budget.
		(Some("0"), None),
		(Some("4096"), Some(4096)),
		(Some(" 4096 "), Some(4096)),
		(Some("18446744073709551615"), Some(u64::MAX)),
	] {
		assert_eq!(maintenance_segment_ceiling_from(raw), want, "{raw:?} parses as {want:?}");
	}
}

#[test]
fn an_absent_or_satisfied_ceiling_cuts_nothing() {
	let ts: Vec<i64> = (0..100).collect();
	assert_eq!(size_driven_cuts(&ts, 1_000_000, None, 8), Vec::<i64>::new(), "no ceiling, no re-split: the default changes nothing");
	assert_eq!(size_driven_cuts(&ts, 1_000_000, Some(0), 8), Vec::<i64>::new(), "a zero ceiling means off, not cut-everywhere");
	assert_eq!(size_driven_cuts(&ts, 999, Some(1000), 8), Vec::<i64>::new(), "an output that already fits is left as one frame");
	assert_eq!(size_driven_cuts(&ts, 1_000, Some(1000), 8), Vec::<i64>::new(), "exactly at the ceiling fits: the ceiling is a maximum, not a trigger");
	assert_eq!(size_driven_cuts(&ts, 1_000_000, Some(1), 1), Vec::<i64>::new(), "a fold replacing one segment is not cut here, because the extra output would need a fresh id (rule P2)");
	assert_eq!(size_driven_cuts(&[], 1_000_000, Some(1), 8), Vec::<i64>::new(), "no rows, nowhere to cut");
	assert_eq!(size_driven_cuts(&[7], 1_000_000, Some(1), 8), Vec::<i64>::new(), "one row, nowhere to cut");
}

#[test]
fn the_cuts_divide_the_row_range_and_never_split_a_run_of_equal_timestamps() {
	let ts: Vec<i64> = (0..100).collect();
	assert_eq!(size_driven_cuts(&ts, 4_000, Some(1_000), 8), vec![25, 50, 75], "four pieces, cut at the row-count quarters");
	// A run of one instant cannot straddle two outputs (design D17): the run moves whole to the
	// later output, so a cut inside it is not a cut at all and dedup drops the duplicates.
	assert_eq!(size_driven_cuts(&[5; 100], 100_000, Some(1), 8), Vec::<i64>::new(), "one instant cannot be divided");
	let plateau: Vec<i64> = (0..100).map(|row| row / 10).collect();
	let cuts = size_driven_cuts(&plateau, 100_000, Some(1), 8);
	assert!(!cuts.is_empty(), "a plateaued range still divides where it has distinct instants");
	assert!(cuts.iter().all(|cut| plateau.contains(cut)), "every cut is an instant the range actually has, so it falls between two instants: {cuts:?}");
	assert!(cuts.iter().all(|cut| *cut > plateau[0]), "no cut at or below the first instant, which would leave an empty output: {cuts:?}");
	assert!(cuts.windows(2).all(|pair| pair[0] < pair[1]), "the cuts come out sorted and deduplicated: {cuts:?}");
	for cut in &cuts {
		let prefix = plateau.partition_point(|&t| t < *cut);
		assert!(prefix > 0 && prefix < plateau.len(), "cut {cut} leaves both sides non-empty, or SwapPlan::cut drops it silently");
	}
}

#[test]
fn a_fold_is_never_cut_into_more_outputs_than_the_segments_it_replaces() {
	// The cap is `members.len()`, passed by every caller: with one output per member each output
	// takes a member's id (precedence P1) and none needs a fresh id, so the refusal rule P2
	// documents cannot fire. An arbitrarily small ceiling must therefore still respect it.
	let ts: Vec<i64> = (0..1000).collect();
	assert_eq!(size_driven_cuts(&ts, u64::MAX, Some(1), 3).len(), 2, "three members, three outputs, two cuts, however small the ceiling");
	assert_eq!(size_driven_cuts(&ts, u64::MAX, Some(1), 2).len(), 1, "two members cannot be cut deeper than that");
	assert_eq!(size_driven_cuts(&ts, 8_000, Some(1_000), 100).len(), 7, "eight pieces wanted, and the members allow eight: seven cuts");
}

#[tokio::test]
async fn a_fold_over_the_ceiling_divides_its_output_rather_than_writing_one_frame() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), ceiling(1)).await;
	seal_two_overlapping(&store).await;
	let merged = store.reconcile_overlaps(ASPECT).await.expect("a ceiling bounds the output, it does not refuse the fold");
	let rows = store.index.rows(ASPECT).await.expect("read the index");
	assert_eq!(rows.len(), 2, "the fold wrote two segments where it would have written one, reported {merged}");
	assert_eq!(rows.iter().map(|row| row.desc.row_count).sum::<usize>(), 1_500, "dividing the output folded nothing away: 0..1500 with 500 shared instants is 1500 rows");
	let pieces: Vec<u64> = rows.iter().map(|row| row.desc.byte_len).collect();
	assert!(pieces.iter().all(|piece| *piece > 0), "both pieces were written: {pieces:?}");
	drop(store);
}

#[tokio::test]
async fn the_same_fold_writes_one_frame_when_the_ceiling_is_off() {
	// The control for the test above, on the identical fixture: the ceiling is the only thing
	// that differs, so any difference in the segment count belongs to it.
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), |options| options).await;
	seal_two_overlapping(&store).await;
	store.reconcile_overlaps(ASPECT).await.expect("an unbounded fold");
	let rows = store.index.rows(ASPECT).await.expect("read the index");
	assert_eq!(rows.len(), 1, "no ceiling, one folded frame");
	assert_eq!(rows[0].desc.row_count, 1_500);
	drop(store);
}

#[tokio::test]
async fn a_ceiling_far_above_the_fold_writes_what_an_unbounded_fold_writes() {
	let dir = TempDir::new().unwrap();
	let store = store_at(dir.path(), ceiling(1 << 40)).await;
	seal_two_overlapping(&store).await;
	store.reconcile_overlaps(ASPECT).await.expect("the group is far under the ceiling");
	let rows = store.index.rows(ASPECT).await.expect("read the index");
	assert_eq!(rows.len(), 1, "a ceiling the fold fits under is indistinguishable from no ceiling");
	assert_eq!(rows[0].desc.row_count, 1_500);
	drop(store);
}
