//! TAG-6, the maintenance lane of tagged series: every maintenance pass plans within one
//! series. Overlap components, squashes, size-targeted groups and hot tails are taken per
//! series, the gates (`squash_aspect_if_exceeds`, the fragmentation gate, the overlap sweep's
//! `had_overlap`) count per series, and the overlap statistics sum per-series indexes. An
//! untagged aspect is a single series, so it plans exactly as before (the existing
//! maintenance, crash-matrix and compat tests are the proof). Tagged batches seal through the
//! public `ingest`, whose library gate opened in TAG-8.

use std::{collections::BTreeMap, path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType, SplitPolicy};

use super::{merge_rows_per_page, IndexRow, MaintenanceWait, SegmentStore, SwapPlan};
use crate::{IngestBatch, TagSet};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

/// A lossless f64 schema in seconds.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// The aspect most tests here write.
const ASPECT: &str = "price";

fn tags(host: &str) -> TagSet {
	TagSet::from_pairs([("host", host)]).expect("valid tags")
}

/// A fresh store at `root` with `aspects` declared.
async fn store_with(root: &Path, aspects: &[&str]) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	for aspect in aspects {
		store.declare(aspect, &schema()).await.expect("declares");
	}
	store
}

/// Seal `timestamps` (value = timestamp) into `aspect` as one segment of `groups`' series
/// (the empty tag set is untagged), returning the new segment's id.
async fn seal(store: &SegmentStore, aspect: &str, groups: &TagSet, timestamps: &[i64]) -> u64 {
	static KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
	let key = format!("{aspect}-{}", KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
	let values = timestamps.iter().map(|t| BigDecimal::from(*t)).collect();
	let batch = IngestBatch::dense(key, timestamps.to_vec(), values).with_groups(groups.clone());
	let receipt = store.ingest(aspect, &batch).await.expect("ingests");
	assert_eq!(receipt.segment_ids.len(), 1, "one segment per seal");
	receipt.segment_ids[0]
}

/// The rows of `aspect`, ascending by id.
async fn rows_of(store: &SegmentStore, aspect: &str) -> Vec<IndexRow> {
	store.index.rows(aspect).await.expect("reads the rows")
}

/// The segment count of each series of `aspect`.
async fn per_series(store: &SegmentStore, aspect: &str) -> BTreeMap<u64, usize> {
	let mut counts = BTreeMap::new();
	for row in rows_of(store, aspect).await {
		*counts.entry(row.series_id).or_default() += 1;
	}
	counts
}

/// Every timestamp a series of `aspect` holds, read back through its frames.
async fn series_timestamps(store: &SegmentStore, aspect: &str, series_id: u64) -> Vec<i64> {
	let mut out = Vec::new();
	for row in rows_of(store, aspect).await.into_iter().filter(|row| row.series_id == series_id) {
		out.extend(store.read_bound_frame(&row).await.expect("reads the frame").timestamps);
	}
	out.sort_unstable();
	out
}

/// I12: a swap plan whose members belong to two series is refused before anything is
/// written, and the rows stay as they were.
#[tokio::test]
async fn swap_plan_refuses_mixed_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	seal(&store, ASPECT, &tags("a"), &[0, 10]).await;
	seal(&store, ASPECT, &tags("b"), &[5, 15]).await;
	let before = rows_of(&store, ASPECT).await;
	assert_eq!(before.iter().map(|row| row.series_id).collect::<Vec<_>>(), vec![1, 2]);
	let held = store.maintain(ASPECT).await.expect("holds");
	let (ts, vs) = store.merge_members(&before).await.expect("reads the members");
	let rows_per_page = merge_rows_per_page(ts.len());
	let plan = SwapPlan::cut(before.clone(), ts, vs, rows_per_page, &[]);
	let Err(err) = store.swap_plan(&held, &schema(), plan).await else { panic!("a plan mixing series is refused") };
	assert!(format!("{err:#}").contains("mixes the series"), "{err:#}");
	drop(held);
	assert_eq!(rows_of(&store, ASPECT).await, before, "nothing was swapped");
	drop(store);
	let frames = std::fs::read_dir(dir.path().join("segments")).expect("lists").filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "weftseg")).count();
	assert_eq!(frames, before.len(), "no output frame was left behind");
}

/// Overlap components are taken per series: two series whose windows overlap are never
/// merged together, while two overlapping segments of one series still are.
#[tokio::test]
async fn reconcile_overlaps_never_merges_across_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	seal(&store, ASPECT, &tags("a"), &[0, 10]).await;
	seal(&store, ASPECT, &tags("b"), &[0, 10]).await;
	seal(&store, ASPECT, &tags("a"), &[5, 15]).await;
	seal(&store, ASPECT, &TagSet::new(), &[3, 8]).await;
	assert_eq!(store.reconcile_overlaps(ASPECT).await.expect("reconciles"), 1, "only series a's two segments merge");
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(0, 1), (1, 1), (2, 1)]));
	assert_eq!(series_timestamps(&store, ASPECT, 1).await, vec![0, 5, 10, 15]);
	assert_eq!(series_timestamps(&store, ASPECT, 2).await, vec![0, 10]);
	assert_eq!(series_timestamps(&store, ASPECT, 0).await, vec![3, 8]);
	assert_eq!(store.reconcile_overlaps(ASPECT).await.expect("reconciles"), 0, "a second pass finds nothing");
}

/// The overlap statistics count per series: two series sharing a window are not overlap,
/// and the overlap sweep does not count such an aspect as reconciled.
#[tokio::test]
async fn two_series_sharing_a_window_are_not_overlapping() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	seal(&store, ASPECT, &tags("a"), &[0, 10]).await;
	seal(&store, ASPECT, &tags("b"), &[0, 10]).await;
	seal(&store, ASPECT, &TagSet::new(), &[0, 10]).await;
	assert_eq!(store.aspect_stats(ASPECT).await.expect("stats").overlapping_segments, 0);
	assert_eq!(store.store_overlapping_segments().await.expect("stats"), 0);
	let sweep = store.reconcile_all_overlaps(MaintenanceWait::Skip).await.expect("sweeps");
	assert_eq!((sweep.aspects_reconciled, sweep.segments_removed), (0, 0), "nothing overlapped, so nothing was reconciled");
	// Within one series, overlap still counts.
	seal(&store, ASPECT, &tags("a"), &[5, 15]).await;
	assert_eq!(store.aspect_stats(ASPECT).await.expect("stats").overlapping_segments, 2);
	assert_eq!(store.store_overlapping_segments().await.expect("stats"), 2);
	let sweep = store.reconcile_all_overlaps(MaintenanceWait::Skip).await.expect("sweeps");
	drop(store);
	assert_eq!((sweep.aspects_reconciled, sweep.segments_removed), (1, 1));
}

/// A squash folds each series into its own output, and the threshold gate counts per
/// series: it fires only for a series over the cap and squashes only that series.
#[tokio::test]
async fn squash_writes_one_output_per_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	for t in [0, 10, 20] {
		seal(&store, ASPECT, &tags("a"), &[t]).await;
		seal(&store, ASPECT, &tags("b"), &[t]).await;
	}
	seal(&store, ASPECT, &TagSet::new(), &[1]).await;
	seal(&store, ASPECT, &TagSet::new(), &[2]).await;
	// 8 segments, 3 + 3 + 2 per series: no series is over a cap of 3.
	assert_eq!(store.squash_aspect_if_exceeds(ASPECT, 3).await.expect("checks"), None, "the cap is per series");
	assert_eq!(store.squash_aspect_if_exceeds(ASPECT, 2).await.expect("squashes"), Some(4), "series a and b fold, the untagged pair stays");
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(0, 2), (1, 1), (2, 1)]));
	assert_eq!(store.squash_aspect(ASPECT).await.expect("squashes"), 1, "the untagged pair folds too");
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(0, 1), (1, 1), (2, 1)]));
	assert_eq!(series_timestamps(&store, ASPECT, 1).await, vec![0, 10, 20]);
	assert_eq!(series_timestamps(&store, ASPECT, 2).await, vec![0, 10, 20]);
	assert_eq!(series_timestamps(&store, ASPECT, 0).await, vec![1, 2]);
	assert_eq!(store.squash_aspect(ASPECT).await.expect("squashes"), 0, "one segment per series is fully squashed");
}

/// Size-targeted compaction groups consecutive ids within a series, never across, and the
/// fragmentation gate's ideal counts at least one segment per series.
#[tokio::test]
async fn target_rows_groups_within_a_series_only() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	// Interleaved ids a, b, a, b: an aspect-wide grouping at 2 rows would pair a with b.
	seal(&store, ASPECT, &tags("a"), &[0]).await;
	seal(&store, ASPECT, &tags("b"), &[0]).await;
	seal(&store, ASPECT, &tags("a"), &[10]).await;
	seal(&store, ASPECT, &tags("b"), &[10]).await;
	assert_eq!(store.squash_aspect_to_target_rows(ASPECT, 2).await.expect("compacts"), 2);
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(1, 1), (2, 1)]));
	assert_eq!(series_timestamps(&store, ASPECT, 1).await, vec![0, 10]);
	assert_eq!(series_timestamps(&store, ASPECT, 2).await, vec![0, 10]);
	// 4 rows in 2 segments at a target of 4: the aspect-wide ideal is 1, but each series
	// needs its own segment, so the gate holds.
	assert_eq!(store.squash_aspect_to_target_rows_if_fragmented(ASPECT, 4).await.expect("checks"), None, "one segment per series is as compact as it gets");
	let sweep = store.squash_all_to_target_rows_if_fragmented(4, MaintenanceWait::Skip).await.expect("sweeps");
	assert_eq!((sweep.aspects_squashed, sweep.segments_removed), (0, 0));
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(1, 1), (2, 1)]));
	drop(store);
}

/// Every series keeps its own newest segment hot under the hot/cold reconcile, while the
/// backlog threshold stays aspect-wide.
#[tokio::test]
async fn hot_cold_keeps_every_series_tail_hot() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	let a_cold = seal(&store, ASPECT, &tags("a"), &[20, 0, 10]).await;
	let b_tail = seal(&store, ASPECT, &tags("b"), &[20, 0, 10]).await;
	let a_tail = seal(&store, ASPECT, &tags("a"), &[50, 30, 40]).await;
	seal(&store, ASPECT, &TagSet::new(), &[0, 1]).await;
	let unsorted = |rows: &[IndexRow]| rows.iter().filter(|row| !row.desc.time_sorted).map(|row| row.desc.id).collect::<Vec<_>>();
	assert_eq!(unsorted(&rows_of(&store, ASPECT).await), vec![a_cold, b_tail, a_tail], "three out-of-order segments");
	// The aspect's newest segment is the untagged one; per series, b's only segment and a's
	// second are the tails. Below the threshold only a's first is cold.
	let outcome = store.reconcile_aspect_hot_cold(ASPECT, 10).await.expect("reconciles");
	assert_eq!((outcome.cold_reconciled, outcome.hot_reconciled), (1, 0));
	assert_eq!(unsorted(&rows_of(&store, ASPECT).await), vec![b_tail, a_tail], "both series tails stay hot");
	// At the aspect-wide threshold the tails are reconciled too.
	let outcome = store.reconcile_aspect_hot_cold(ASPECT, 2).await.expect("reconciles");
	assert_eq!((outcome.cold_reconciled, outcome.hot_reconciled), (0, 2));
	assert_eq!(unsorted(&rows_of(&store, ASPECT).await), [] as [u64; 0]);
	assert_eq!(per_series(&store, ASPECT).await, BTreeMap::from([(0, 1), (1, 2), (2, 1)]), "a reconcile keeps each segment's series");
	drop(store);
}

/// A split's suffix takes a fresh id; only a newer overlapping segment of the *same*
/// series refuses it (precedence is per series), so a newer segment of another series in
/// the same window does not.
#[tokio::test]
async fn split_is_refused_only_by_a_newer_segment_of_its_own_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_with(dir.path(), &[ASPECT]).await;
	let a = seal(&store, ASPECT, &tags("a"), &[0, 10, 20, 30]).await;
	seal(&store, ASPECT, &tags("b"), &[0, 10, 20, 30]).await;
	let suffix = store.split_segment(ASPECT, a, 15).await.expect("another series never outranks").expect("carved");
	assert_eq!(series_timestamps(&store, ASPECT, 1).await, vec![0, 10, 20, 30]);
	assert_eq!(rows_of(&store, ASPECT).await.iter().find(|row| row.desc.id == suffix).map(|row| row.series_id), Some(1));
	seal(&store, ASPECT, &tags("a"), &[40, 50, 60]).await;
	let newest = rows_of(&store, ASPECT).await.into_iter().filter(|row| row.series_id == 1).map(|row| row.desc.id).max().expect("a row");
	seal(&store, ASPECT, &tags("a"), &[45, 65]).await;
	let err = store.split_segment(ASPECT, newest, 55).await.expect_err("a newer overlapping segment of the series refuses");
	drop(store);
	assert!(format!("{err:#}").contains("outrank"), "{err:#}");
}

/// The store-wide equivalent of weft-server's `every_tick_kind_isolates_a_truncated_frame`
/// with a two-series aspect beside it: each sweep the daemon runs fails only the aspect with
/// the truncated frame, still maintains the tagged aspect, and never folds its series together.
#[tokio::test]
async fn every_sweep_kind_isolates_a_truncated_frame_beside_a_two_series_aspect() {
	for kind in ["threshold", "hot_cold", "overlaps", "overlaps_split", "squash", "compact"] {
		let dir = TempDir::new().expect("tempdir");
		let store = store_with(dir.path(), &["s1", "s2", "s3"]).await;
		for aspect in ["s1", "s2"] {
			let first = seal(&store, aspect, &TagSet::new(), &[100, 130, 110]).await;
			seal(&store, aspect, &TagSet::new(), &[120, 150, 125]).await;
			seal(&store, aspect, &TagSet::new(), &[140, 170, 145]).await;
			if aspect == "s2" {
				let row = rows_of(&store, aspect).await.into_iter().find(|row| row.desc.id == first).expect("the row");
				let file = std::fs::OpenOptions::new().write(true).open(&row.desc.path).expect("opens");
				let len = file.metadata().expect("stats").len();
				file.set_len(len / 2).expect("truncates");
			}
		}
		// s3: series a has three overlapping out-of-order segments, series b one in the same window.
		seal(&store, "s3", &tags("a"), &[100, 130, 110]).await;
		seal(&store, "s3", &tags("b"), &[100, 170, 110]).await;
		seal(&store, "s3", &tags("a"), &[120, 150, 125]).await;
		seal(&store, "s3", &tags("a"), &[140, 170, 145]).await;
		let wait = MaintenanceWait::Skip;
		let (maintained, failed): (usize, Vec<String>) = match kind {
			"threshold" => store.reconcile_all_over_threshold(1, wait).await.map(|s| (s.aspects_reconciled, s.failed.into_iter().map(|(a, _)| a).collect())),
			"hot_cold" => store.reconcile_all_hot_cold(1, wait).await.map(|s| (s.aspects_reconciled, s.failed.into_iter().map(|(a, _)| a).collect())),
			"overlaps" => store.reconcile_all_overlaps(wait).await.map(|s| (s.aspects_reconciled, s.failed.into_iter().map(|(a, _)| a).collect())),
			"overlaps_split" => store.reconcile_all_overlaps_with_policy(SplitPolicy::new(1), wait).await.map(|s| (s.aspects_reconciled, s.failed.into_iter().map(|(a, _)| a).collect())),
			"squash" => store.squash_all_over_threshold(2, wait).await.map(|s| (s.aspects_squashed, s.failed.into_iter().map(|(a, _)| a).collect())),
			"compact" => store.squash_all_to_target_rows_if_fragmented(6, wait).await.map(|s| (s.aspects_squashed, s.failed.into_iter().map(|(a, _)| a).collect())),
			_ => unreachable!("unknown sweep kind {kind}"),
		}
		.unwrap_or_else(|err| panic!("{kind}: a bad aspect failed the whole sweep: {err:#}"));
		assert_eq!(failed, vec!["s2".to_string()], "{kind}: only the bad aspect fails");
		assert_eq!(maintained, 2, "{kind}: both healthy aspects are maintained");
		let series = per_series(&store, "s3").await;
		assert_eq!(series.keys().copied().collect::<Vec<_>>(), vec![1, 2], "{kind}: both series keep their segments");
		assert_eq!(series[&2], 1, "{kind}: series b is never folded into a");
		assert_eq!(series_timestamps(&store, "s3", 2).await, vec![100, 110, 170], "{kind}: series b keeps exactly its rows");
		let a = series_timestamps(&store, "s3", 1).await;
		drop(store);
		assert_eq!(a, vec![100, 110, 120, 125, 130, 140, 145, 150, 170], "{kind}: series a keeps exactly its rows");
	}
}
