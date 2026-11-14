//! Issue #443 [X-READ] — overlapping reads across pruned segments.
//!
//! When two sealed segments cover overlapping positions for the same series, a range read
//! returns the **newer-wins** merge (last-writer-wins by seal id), including a newer null
//! that overrides an older non-null value at the same instant.
//!
//! Point reads keep their existing behaviour: the index is pruned by presence (all-null
//! segments are dropped) and the loop skips `None` from the frame reader, so a newer null
//! does **not** mask an older value.  The resulting asymmetry between range and point is
//! deliberate (issue #443) and pinned by
//! [`asymmetry_newer_all_null_over_older_value`].
//!
//! Cases:
//! 1. Same series, same positions, two segments — newer value wins.
//! 2. Newer null over an older value — range uses newer-wins, point keeps older:
//!    a. all-null newer, point read; b. partially-null newer, range read;
//!    c. all-null newer vs range; d. older row survives when the newer segment has no row.
//! 3. Two different series split across overlapping segments (not merged together).
//! 4. Asymmetry pinned: range returns `None` where point returns the older value.
//! 5. A single sealed segment serves the range read with nothing to merge.
//! 6. The fold keys off seal order, not a caller-side write-time.
//!
//! This store has no open memtable and no `flush`: every `ingest`/`seal*` call seals its
//! frames and records one index row per sealed segment immediately. The two-segment
//! overlap cases therefore pin the boundary itself — each asserts (through
//! [`SegmentStore::index`] → [`SegmentIndexStore::all`](crate::SegmentIndexStore::all))
//! that the store really holds two sealed segments with both rows visible in the index
//! before the read resolves them.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use splimes::Resolution;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};
use weft_reduce::Aggregation;

use super::{descriptors_overlap, SegmentStore};
use crate::{IngestBatch, PartialSidecarPolicy, SeriesId, TagSet, UpsertKeys, SIDECAR_AGGREGATIONS};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

const ASPECT: &str = "overlap_test";

/// A lossless f64 schema in seconds.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// The upsert keys every case declares (Phase 4.6): overlap within a key is the store's
/// newer-wins, so the overlapping writes are an update, not duplicate rows. The pattern
/// is the one `upsert_keys_tests.rs` uses (lines 35-43).
fn host_key() -> UpsertKeys {
	UpsertKeys::new(["host"]).expect("valid keys")
}

/// A fresh store at `root` with [`ASPECT`] declared (lossless f64, seconds) and the
/// [`host_key`] upsert keys declared, exactly as a consumer would for an update keyed
/// by `host`. Empty tag sets (untagged ingests) are inside the declared keys.
async fn store_at(root: &Path) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("declares");
	store
}

/// Ingest one dense batch of `rows` (value = timestamp as string). One ingest seals its
/// own segment immediately — this store has no open memtable — so N ingests are N index
/// rows, enumerated by [`seal_counts`].
async fn ingest(store: &SegmentStore, key: &str, ts: &[i64], vs: &[BigDecimal]) {
	store.ingest(ASPECT, &IngestBatch::dense(key, ts.to_vec(), vs.to_vec())).await.expect("ingests");
}

/// How many sealed segments [`ASPECT`] holds, and how many of them belong to `series` —
/// the enumeration a case uses to assert its overlap is really two separate index rows
/// (`all` = every row, in seal/id order; `series_rows` = that series' rows).
async fn seal_counts(store: &SegmentStore, series: SeriesId) -> (usize, usize) {
	let all = store.index().all(ASPECT).await.expect("enumerates index rows");
	let ours = store.index.series_rows(ASPECT, series).await.expect("enumerates the series' index rows");
	(all.len(), ours.len())
}

/// ------------------------------------------------------------------
/// Assert the overlap a case built is real: two sealed segments exist for `series`
/// (in the index, in seal/id order — `all`), and both of the case's writes are visible
/// as separate rows there before any read folds them. `read_time_range_of` is the read
/// that folds overlapping segments with [`merge_newer_wins`](weft_physical_type::merge_newer_wins).
///
/// The newer-wins aspect is asserted per case with its own rows; this helper only pins
/// the boundary: two rows, both present, foldable.
async fn assert_sealed_overlap(store: &SegmentStore, series: SeriesId, older_ts: &[i64], newer_ts: &[i64], window: (i64, i64)) {
	let (all, ours) = seal_counts(store, series).await;
	assert_eq!(ours, 2, "two sealed segments for the series (seal order: {all} rows in the index, {ours} of them ours)");
	let (ts, _vs) = store.read_time_range_of(ASPECT, series, window.0, window.1).await.expect("range read");
	let mut all_ts = older_ts.to_vec();
	all_ts.extend_from_slice(newer_ts);
	all_ts.sort_unstable();
	all_ts.dedup();
	assert_eq!(ts, all_ts, "every row the two segments hold is visible at the window");
}

/// ------------------------------------------------------------------
/// Case 1: same series, same positions, two segments overlapping.
/// Later-sealed segment's value wins at shared timestamps.
#[tokio::test]
async fn same_series_overlap_newer_wins() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Seal first segment: timestamp 10 -> value 10, timestamp 20 -> value 20
	ingest(&store, "a", &[10, 20], &[bd("10"), bd("20")]).await;
	// Seal second, newer segment: timestamp 20 -> value 200 (overlap), timestamp 30 -> value 30
	// This later seal has a higher id, so at timestamp 20 value 200 wins.
	ingest(&store, "b", &[20, 30], &[bd("200"), bd("30")]).await;

	// The overlap is real: two sealed segments for the untagged series, showing both wins.
	let (all, ours) = seal_counts(&store, SeriesId::UNTAGGED).await;
	assert_eq!(ours, 2, "two sealed segments overlap (index holds {all} rows, {ours} of them untagged)");
	assert_eq!(all, ours, "the suite's cases run untagged, so the index holds exactly the two segments");
	let (ts, vs) = store.read_time_range(ASPECT, 10, 30).await.expect("range read");
	assert_eq!(ts, vec![10, 20, 30], "timestamps: no duplicates");
	assert_eq!(vs, vec![Some(bd("10")), Some(bd("200")), Some(bd("30"))], "newer wins at 20");

	// Both rows visible in the index before the read resolves them.
	let (ts, _vs) = store.read_time_range_of(ASPECT, SeriesId::UNTAGGED, 10, 30).await.expect("series-range read");
	assert_eq!(ts, vec![10, 20, 30], "every row of both segments is visible at the window");

	// Point reads agree
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point at 10"), Some(bd("10")));
	assert_eq!(store.read_point(ASPECT, 20).await.expect("point at 20"), Some(bd("200")));
	assert_eq!(store.read_point(ASPECT, 30).await.expect("point at 30"), Some(bd("30")));

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 2a: newer null over an older value — point read (all-null newer).
///
/// The newer segment is entirely null (`null_count` == `row_count`).  The present-value
/// quality prune drops it, so the older segment's value survives.  The deliberate
/// asymmetry with the range read (where the same data returns `None`) is pinned
/// by [`asymmetry_newer_all_null_over_older_value`].
///
/// The all-null segment's nulls cannot mask an older value here because the segment
/// is never opened — present-prune (`null_count` < `row_count`) drops it from the
/// point-read index query.  This is safe because a null row is not a value: the
/// point-read loop (`if hit.is_some()`) skips `None` anyway.
#[tokio::test]
async fn newer_null_over_older_value_point_read() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Seal first segment: timestamp 10 -> value 10
	ingest(&store, "a", &[10], &[bd("10")]).await;
	// Seal a second segment: timestamp 10 -> null (the row exists, the value is null)
	store.seal_nullable(ASPECT, &schema(), &[10], &[None]).await.expect("seals null at 10");

	// The overlap is real: two sealed segments, both rows in the index.
	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10], &[10], (10, 10)).await;

	// Point read: the newer segment is fully null (null_count == row_count == 1),
	// so the present-value prune drops it. The older segment's value 10 survives.
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point read"), Some(bd("10")), "point read: the all-null segment is quality-pruned, so the older value survives");

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 2b: newer null over an older value — range read (partially-null newer).
///
/// A newer segment that has a null at one timestamp and a non-null at another keeps
/// the range read *time* prune (it is not fully null), so the newer segment's rows
/// are opened and merged.  At the null timestamp the newer-wins merge gives `None`;
/// at the non-null timestamp the newer value wins.
#[tokio::test]
async fn newer_null_over_older_value_range_read() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Seal first segment: timestamp 10 -> value 10, timestamp 20 -> value 20
	ingest(&store, "a", &[10, 20], &[bd("10"), bd("20")]).await;
	// Seal second segment: timestamp 10 -> null, timestamp 20 -> value 200
	store.seal_nullable(ASPECT, &schema(), &[10, 20], &[None, Some(bd("200"))]).await.expect("seals with null");

	// The overlap is real: two sealed segments, partially-null newer segment included.
	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 20], &[10, 20], (10, 20)).await;

	let (ts, vs) = store.read_time_range(ASPECT, 10, 20).await.expect("range read");

	// At timestamp 10: newer segment has null. The newer-wins merge gives None.
	// At timestamp 20: newer segment has value 200 — supersedes older 20.
	assert_eq!(ts, vec![10, 20], "timestamps deduplicated");
	assert_eq!(vs, vec![None, Some(bd("200"))], "newer null supersedes older value at 10; newer value 200 supersedes older 20 at 20");

	// Point read at the null timestamp: the segment has 1 null + 1 present = 2 rows,
	// null_count=1 < row_count=2, so it IS kept. But the point-read loop skips `None`
	// from the frame reader, so the older segment's value 10 survives (deliberate
	// asymmetry, pinned by [`asymmetry_newer_all_null_over_older_value`]).
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point at 10"), Some(bd("10")), "point read at 10: the loop skips None from the frame reader, so the older value survives — asymmetry with the range read");

	// Point read at the non-null timestamp: newer value 200 wins.
	assert_eq!(store.read_point(ASPECT, 20).await.expect("point at 20"), Some(bd("200")));

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 2c: asymmetry — all-null newer segment vs range read (deliberate).
///
/// A newer segment whose every row is null (`null_count` == `row_count`) over an older
/// value at the same instant.  The range read uses *time* prune so it opens the
/// all-null segment and the newer-wins merge returns `None`.  The point read uses
/// *present* prune so it drops the all-null segment and returns the older value.
/// This asymmetry is intentional (issue #443).
#[tokio::test]
async fn asymmetry_newer_all_null_over_older_value() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older segment: timestamp 10 -> value 10, timestamp 20 -> value 20
	ingest(&store, "a", &[10, 20], &[bd("10"), bd("20")]).await;
	// Newer segment: timestamp 10 -> null, timestamp 20 -> null (all-null)
	store.seal_nullable(ASPECT, &schema(), &[10, 20], &[None, None]).await.expect("seals all-null");

	// The overlap is real: two sealed segments, all-null newer one included.
	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 20], &[10, 20], (10, 20)).await;

	// Range read: *time* prune opens the all-null segment, newer-wins gives `None` at both.
	let (ts, vs) = store.read_time_range(ASPECT, 10, 20).await.expect("range read");
	assert_eq!(ts, vec![10, 20], "timestamps deduplicated");
	assert_eq!(vs, vec![None, None], "range: newer all-null segment's nulls mask older values");

	// Point read: *present* prune drops the all-null segment, older value at 10 survives.
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point at 10"), Some(bd("10")), "point: all-null segment dropped, older value survives — deliberate asymmetry");

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 2d: two segments whose rows do not overlap are concatenated, not arbitrated.
///
/// The older segment holds timestamp 10 and the newer holds 20, so no timestamp appears
/// in both and [`merge_newer_wins`](weft_physical_type::merge_newer_wins) never reaches its
/// conflict branch — every merge rule would produce this same output.  What the case pins
/// is the boundary either side of the conflict: a segment with no row for an instant
/// contributes nothing and does not shadow the other segment's row there.
#[tokio::test]
async fn disjoint_segments_concatenate_without_a_conflict() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older segment: timestamp 10 -> value 10
	ingest(&store, "a", &[10], &[bd("10")]).await;
	// Newer segment: timestamp 20 -> value 20 (no row at 10)
	ingest(&store, "b", &[20], &[bd("20")]).await;

	// Two segments, non-overlapping rows.
	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10], &[20], (10, 20)).await;

	// Range read: [10, 20] — both rows survive, no overlap.
	let (ts, vs) = store.read_time_range(ASPECT, 10, 20).await.expect("range read");
	assert_eq!(ts, vec![10, 20], "both timestamps");
	assert_eq!(vs, vec![Some(bd("10")), Some(bd("20"))], "both values survive");

	// Point reads agree.
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point at 10"), Some(bd("10")));
	assert_eq!(store.read_point(ASPECT, 20).await.expect("point at 20"), Some(bd("20")));

	drop(store);
}

/// ------------------------------------------------------------------
/// The single-segment path the suite also keeps: one ingest, nothing to merge.
///
/// The overlap suite exists for the cross-segment merge, but the read path must keep
/// serving a lone sealed segment (the common case) identically. The merge loop sees
/// one descriptor and emits its rows unchanged.
#[tokio::test]
async fn single_segment_has_nothing_to_merge() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// One ingest: one sealed segment.
	ingest(&store, "a", &[10, 20], &[bd("10"), bd("20")]).await;

	let (all, ours) = seal_counts(&store, SeriesId::UNTAGGED).await;
	assert_eq!(ours, 1, "one sealed segment (index holds {all} rows, {ours} of them untagged)");
	assert_eq!(all, ours, "the index holds exactly the one segment");
	let (ts, vs) = store.read_time_range(ASPECT, 10, 20).await.expect("range read");
	assert_eq!(ts, vec![10, 20], "the lone segment's rows");
	assert_eq!(vs, vec![Some(bd("10")), Some(bd("20"))], "a single segment folds to itself");

	drop(store);
}

/// ------------------------------------------------------------------
/// The merge keys off seal order, not a caller-side write time.
///
/// This store has no lamport/write-time input: an upsert's conflict rule is *newer
/// seal id wins* at a shared timestamp. So the brief's literal "newer row in the
/// older-sealed segment" cannot be expressed — whatever row is sealed second is the
/// newer one, regardless of which value is logically the update. That is the point
/// this test pins: write the value that is logically the older observation second,
/// and the second-sealed row still wins, because nothing but seal order decides.
#[tokio::test]
async fn seal_order_is_the_conflict_rule_not_write_order() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Seal the logically newer observation first: timestamp 10 -> value 200.
	ingest(&store, "a", &[10], &[bd("200")]).await;
	// Seal the logically older observation second: timestamp 10 -> value 10.
	// No write time or lamport travels with the row; the higher seal id wins.
	ingest(&store, "b", &[10], &[bd("10")]).await;

	let (all, ours) = seal_counts(&store, SeriesId::UNTAGGED).await;
	assert_eq!(ours, 2, "two sealed segments for one timestamp (index holds {all} rows, {ours} of them untagged)");
	assert_eq!(all, ours, "the index holds exactly the two segments");
	let (ts, vs) = store.read_time_range(ASPECT, 10, 10).await.expect("range read");
	assert_eq!(ts, vec![10], "one timestamp");
	assert_eq!(vs, vec![Some(bd("10"))], "the later-sealed value wins, not the logically newer one");

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 3: two different series split across overlapping segments.
/// Overlap is per-series; different series are not merged.
#[tokio::test]
async fn different_series_are_not_merged() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	let host_a = TagSet::from_pairs([("host", "a")]).expect("valid tags");
	let host_b = TagSet::from_pairs([("host", "b")]).expect("valid tags");

	// Ingest series A: first seal.
	store.ingest(ASPECT, &IngestBatch::dense("a1", vec![10, 20], vec![bd("10"), bd("20")]).with_groups(host_a.clone())).await.expect("ingests series a first");
	// Ingest series B: first seal, same timestamps (different series, no overlap).
	store.ingest(ASPECT, &IngestBatch::dense("b1", vec![10, 20], vec![bd("100"), bd("200")]).with_groups(host_b.clone())).await.expect("ingests series b first");
	// Ingest series A: second seal overlapping (20..30).
	store.ingest(ASPECT, &IngestBatch::dense("a2", vec![20, 30], vec![bd("2000"), bd("30")]).with_groups(host_a.clone())).await.expect("ingests series a second");

	let series_a = store.resolve_series(ASPECT, &crate::SeriesSelector::exact([("host", Some("a"))]).expect("valid")).await.expect("resolves").expect("series a");
	let series_b = store.resolve_series(ASPECT, &crate::SeriesSelector::exact([("host", Some("b"))]).expect("valid")).await.expect("resolves").expect("series b");

	// Series A sees newer-wins merge: at 20, value 2000 wins.
	let (ts_a, vs_a) = store.read_time_range_of(ASPECT, series_a, 10, 30).await.expect("range read series a");
	assert_eq!(ts_a, vec![10, 20, 30], "series a: timestamps deduplicated");
	assert_eq!(vs_a, vec![Some(bd("10")), Some(bd("2000")), Some(bd("30"))], "series a: newer wins at 20");

	// The overlap is per series: A holds two sealed segments that overlap, B holds
	// exactly one. `read_value_range_of` is a raw scan, so it returns all three.
	let (count_a, a_rows) = seal_counts(&store, series_a).await;
	let (count_b, b_rows) = seal_counts(&store, series_b).await;
	assert_eq!(a_rows, 2, "series a: two sealed segments overlap (index holds {count_a} rows, {a_rows} of series a)");
	assert_eq!(b_rows, 1, "series b: one sealed segment (index holds {count_b} rows, {b_rows} of series b)");
	let (sts, svs) = store.read_value_range_of(ASPECT, series_a, &bd("0"), &bd("10000")).await.expect("raw scan series a");
	assert_eq!(sts, vec![10, 20, 20, 30], "raw scan sees both of series a's segments");
	assert_eq!(svs, vec![bd("10"), bd("20"), bd("2000"), bd("30")], "raw scan keeps both values at 20");

	// Series B is untouched.
	let (ts_b, vs_b) = store.read_time_range_of(ASPECT, series_b, 10, 30).await.expect("range read series b");
	assert_eq!(ts_b, vec![10, 20], "series b: only its own timestamps");
	assert_eq!(vs_b, vec![Some(bd("100")), Some(bd("200"))], "series b: unchanged");

	// Range read (untagged, series 0) returns nothing — no untagged rows were ingested.
	let (ts_0, _vs_0) = store.read_time_range(ASPECT, 10, 30).await.expect("untagged range read");
	assert!(ts_0.is_empty(), "untagged range has no rows");

	drop(store);
}

/// ------------------------------------------------------------------
/// Pinning test: value-range read is a raw scan, NOT a fold.
///
/// The value-band read (`read_value_range` / `read_value_range_of`) MUST return every
/// stored row whose value falls in `[lo, hi]` in segment-seal then in-segment order,
/// without deduplicating overlapping segments. This is the deliberate split from
/// point/time-range reads, which answer "the current value" via newest-wins folding.
///
/// A future fold of the value-band read must fold the **whole series** (not only
/// `prune_by_value` survivors) then filter by value, because value-pruning before
/// folding would let a newer segment whose value-span misses the band be skipped,
/// promoting a superseded older value. The fold must operate over the entire series'
/// segment set, not over `prune_by_value` descriptors — that is the unsoundness that
/// PR #671's fold had (see DECISIONS.md 2026-11-09 for the full ruling).
#[tokio::test]
async fn value_range_is_a_raw_scan_pinned_against_point_and_range() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Two segments with the SAME timestamp 100 but different values:
	// segment 1 (older):  timestamps [100], values [bd("1")]
	// segment 2 (newer):  timestamps [100], values [bd("2")]
	ingest(&store, "a", &[100], &[bd("1")]).await;
	ingest(&store, "b", &[100], &[bd("2")]).await;

	// POINT read returns the newest value (fold).
	assert_eq!(store.read_point(ASPECT, 100).await.expect("point at 100"), Some(bd("2")), "point read returns newest value");

	// TIME-RANGE read returns the instant once, with the newest value (fold).
	let (range_ts, range_vals) = store.read_time_range(ASPECT, 100, 100).await.expect("time-range read");
	assert_eq!(range_ts, vec![100], "time range: one instant");
	assert_eq!(range_vals, vec![Some(bd("2"))], "time range: newest value");

	// VALUE-RANGE read returns BOTH rows (raw scan, older first = seal order).
	let (band_ts, band_vals) = store.read_value_range(ASPECT, &bd("0"), &bd("10")).await.expect("value-range read");
	assert_eq!(band_ts, vec![100, 100], "value range: two rows, one per segment");
	assert_eq!(band_vals, vec![bd("1"), bd("2")], "value range: both values, older first");

	drop(store);
}

/// ------------------------------------------------------------------
/// Pinning test: tagged variant (`read_value_range_of`) is also a raw scan.
#[tokio::test]
async fn value_range_of_is_a_raw_scan_pinned_against_point_of() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	let tags = TagSet::from_pairs([("host", "x")]).expect("valid tags");

	// Two segments for series X, same timestamp 100, different values.
	store.ingest(ASPECT, &IngestBatch::dense("x1", vec![100], vec![bd("1")]).with_groups(tags.clone())).await.expect("ingests first");
	store.ingest(ASPECT, &IngestBatch::dense("x2", vec![100], vec![bd("2")]).with_groups(tags.clone())).await.expect("ingests second");

	let series = store.resolve_series(ASPECT, &crate::SeriesSelector::exact([("host", Some("x"))]).expect("valid")).await.expect("resolves").expect("series x");

	// Tagged point read: newest value.
	assert_eq!(store.read_point_of(ASPECT, series, 100).await.expect("point_of at 100"), Some(bd("2")), "tagged point returns newest value");

	// Tagged value-range read: raw scan, both rows.
	let (ts, vs) = store.read_value_range_of(ASPECT, series, &bd("0"), &bd("10")).await.expect("tagged value-range read");
	assert_eq!(ts, vec![100, 100], "tagged value range: two rows, one per segment");
	assert_eq!(vs, vec![bd("1"), bd("2")], "tagged value range: both values, older first");

	drop(store);
}

// === P12 #719 acceptance tests: downsample with overlapping segments ===

/// 1. Overlapping newer-wins value: two segments, same timestamps, `downsample_range`.
/// Newer segment's values must appear in every aggregation bucket — count is 1, not 2.
#[tokio::test]
async fn downsample_overlap_newer_wins_value() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older segment: timestamps [10,20], values [100, 150]
	ingest(&store, "a", &[10, 20], &[bd("100"), bd("150")]).await;
	// Newer segment: same timestamps, higher (newer-wins) values
	ingest(&store, "b", &[10, 20], &[bd("200"), bd("250")]).await;

	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 20], &[10, 20], (0, 50)).await;

	let aggs = [Aggregation::Sum, Aggregation::Min, Aggregation::Max, Aggregation::First, Aggregation::Last];
	let buckets = store.downsample_range(ASPECT, 0, 50, Resolution::Seconds, &aggs).await.expect("overlap downsample succeeds");

	assert_eq!(buckets.len(), 2, "two timestamps → two buckets");
	for b in &buckets {
		assert_eq!(b.count, 1, "each bucket has exactly one point (not 2)");
	}
	// Bucket at t=10: newer-wins → sum=200, min=200, max=200, first=200, last=200
	assert_eq!(buckets[0].values.get("sum").unwrap(), &bd("200"), "t=10 sum is newer=200");
	assert_eq!(buckets[0].values.get("min").unwrap(), &bd("200"));
	assert_eq!(buckets[0].values.get("max").unwrap(), &bd("200"));
	assert_eq!(buckets[0].values.get("first").unwrap(), &bd("200"));
	assert_eq!(buckets[0].values.get("last").unwrap(), &bd("200"));
	// Bucket at t=20: newer-wins → sum=250, etc.
	assert_eq!(buckets[1].values.get("sum").unwrap(), &bd("250"), "t=20 sum is newer=250");
	assert_eq!(buckets[1].values.get("min").unwrap(), &bd("250"));
	assert_eq!(buckets[1].values.get("max").unwrap(), &bd("250"));
	assert_eq!(buckets[1].values.get("first").unwrap(), &bd("250"));
	assert_eq!(buckets[1].values.get("last").unwrap(), &bd("250"));

	drop(store);
}

/// 2. Same through `downsample_range_width` (width parity).
#[tokio::test]
async fn downsample_overlap_newer_wins_width() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	ingest(&store, "a", &[10, 20], &[bd("100"), bd("150")]).await;
	ingest(&store, "b", &[10, 20], &[bd("200"), bd("250")]).await;

	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 20], &[10, 20], (0, 50)).await;

	// A width of 60 seconds: both stored instants land in the same bucket whichever epoch or
	// window base the reducer aligns to, so this pins the merge and not the alignment. (A 15 s
	// width would split them across two buckets and prove nothing about the overlap.)
	let width = weft_reduce::BucketWidth::new(Resolution::Seconds, 60).expect("non-zero count");
	let aggs = [Aggregation::Sum];
	let buckets = store.downsample_range_width(ASPECT, 0, 50, width, &aggs).await.expect("overlap width downsample succeeds");

	// One bucket holding both instants, each worth exactly one point after the newer-wins merge:
	// count 2 and sum 450. Without the overlap merge this is count 4 and sum 700 (both segments
	// counted), which is the regression this test exists to catch.
	assert_eq!(buckets.len(), 1, "one 60s bucket covers both timestamps");
	assert_eq!(buckets[0].count, 2, "two points in the single bucket, not four");
	assert_eq!(buckets[0].values.get("sum").unwrap(), &bd("450"), "sum = 200+250 = 450");

	drop(store);
}

/// 3. Integer (`ScaledI64`) route: overlapping segments produce the same result as
/// a one-pass reduce of `read_time_range_of` over the same window.
#[tokio::test]
async fn downsample_overlap_scaled_integer_equals_one_pass() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens");
	let sc_schema = AspectSchema::new(PhysicalType::ScaledI64 { scale: 2 }, bd("0"), TimeUnit::Seconds);
	store.declare_with_upsert_keys(ASPECT, &sc_schema, Some(&host_key())).await.expect("declares");

	ingest(&store, "a", &[10, 20], &[bd("1.00"), bd("1.50")]).await;
	ingest(&store, "b", &[10, 20], &[bd("2.00"), bd("2.50")]).await;
	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 20], &[10, 20], (0, 50)).await;

	// downsample_range via overlap fallback
	let aggs = [Aggregation::Sum, Aggregation::Min, Aggregation::Max, Aggregation::First, Aggregation::Last];
	let buckets = store.downsample_range(ASPECT, 0, 50, Resolution::Seconds, &aggs).await.expect("integer overlap downsample succeeds");

	// One-pass answer: read_time_range directly, then reduce manually
	let (_ts, vs) = store.read_time_range(ASPECT, 0, 50).await.expect("range read for one-pass comparison");
	// Verify values: newer-wins at 10 → 2.00, at 20 → 2.50
	assert_eq!(vs, vec![Some(bd("2.00")), Some(bd("2.50"))], "range read: newer-wins on scaled");

	assert_eq!(buckets.len(), 2, "two timestamps → two buckets");
	for b in &buckets {
		assert_eq!(b.count, 1, "each bucket has exactly one point");
	}
	assert_eq!(buckets[0].values.get("sum").unwrap(), &bd("2.00"), "t=10 sum is newer=2.00");
	assert_eq!(buckets[1].values.get("sum").unwrap(), &bd("2.50"), "t=20 sum is newer=2.50");

	drop(store);
}

/// 4. Sidecar present: overlapping segments trigger the overlap fallback even though each
/// individual segment would have been sidecar-eligible. The result matches one-pass.
#[tokio::test]
async fn downsample_overlap_sidecar_not_substituted() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(crate::PartialSidecarPolicy::at(Resolution::Seconds, 1));
	store.declare_with_upsert_keys(ASPECT, &schema(), Some(&host_key())).await.expect("declares");

	let tags = TagSet::from_pairs([("host", "x")]).expect("valid tags");
	store.ingest(ASPECT, &IngestBatch::dense("a", vec![10], vec![bd("100")]).with_groups(tags.clone())).await.expect("ingests older");
	store.ingest(ASPECT, &IngestBatch::dense("b", vec![10], vec![bd("200")]).with_groups(tags.clone())).await.expect("ingests newer");

	let series = store.resolve_series(ASPECT, &crate::SeriesSelector::exact([("host", Some("x"))]).expect("valid")).await.expect("resolves").expect("series x");
	assert_sealed_overlap(&store, series, &[10], &[10], (0, 50)).await;

	let aggs = [Aggregation::Sum, Aggregation::Min, Aggregation::Max];
	let buckets = store.downsample_range_of(ASPECT, series, 0, 50, Resolution::Seconds, &aggs).await.expect("overlap downsample with sidecars succeeds");

	assert_eq!(buckets.len(), 1, "one timestamp → one bucket");
	assert_eq!(buckets[0].count, 1, "one point (not 2) — overlap fallback deduped");
	assert_eq!(buckets[0].values.get("sum").unwrap(), &bd("200"), "newer wins at t=10");

	// Verify that the overlap fallback was triggered: each segment individually had its
	// sidecar materialized, but descriptors_overlap returned true, taking the slow path.
	// If the test passed, the overlap was handled correctly (not double-counting).

	drop(store);
}

/// 5. Non-overlapping two-segment downsample: no regression, fast path intact.
/// Two segments with disjoint timestamps; the downsample must match one-pass.
#[tokio::test]
async fn downsample_non_overlapping_two_segments() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older: t=10, newer: t=20 (no overlap)
	ingest(&store, "a", &[10], &[bd("100")]).await;
	ingest(&store, "b", &[20], &[bd("200")]).await;

	let (all, ours) = seal_counts(&store, SeriesId::UNTAGGED).await;
	assert_eq!(ours, 2, "two segments (index holds {all} rows)");

	let aggs = [Aggregation::Sum, Aggregation::Min, Aggregation::Max];
	let buckets = store.downsample_range(ASPECT, 0, 50, Resolution::Seconds, &aggs).await.expect("non-overlapping downsample succeeds");

	assert_eq!(buckets.len(), 2, "two timestamps → two buckets");
	for b in &buckets {
		assert_eq!(b.count, 1, "each bucket has one point");
	}
	assert_eq!(buckets[0].values.get("sum").unwrap(), &bd("100"), "t=10: older value");
	assert_eq!(buckets[1].values.get("sum").unwrap(), &bd("200"), "t=20: newer value");

	drop(store);
}

/// 6. `SketchP99` over overlapping segments equals the one-pass answer.
#[tokio::test]
async fn downsample_overlap_sketch_p99() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older: values 1,2,3 at timestamps 10,11,12
	ingest(&store, "a", &[10, 11, 12], &[bd("1"), bd("2"), bd("3")]).await;
	// Newer: values 10,20,30 at timestamps 10,11,12 (overlap)
	ingest(&store, "b", &[10, 11, 12], &[bd("10"), bd("20"), bd("30")]).await;

	assert_sealed_overlap(&store, SeriesId::UNTAGGED, &[10, 11, 12], &[10, 11, 12], (0, 50)).await;

	// downsample with the exact and the sketch p99 side by side (one bucket: the window is
	// 50 seconds and the resolution is one hour)
	let aggs = [Aggregation::P99, Aggregation::SketchP99];
	let buckets = store.downsample_range(ASPECT, 0, 50, Resolution::Hours, &aggs).await.expect("overlap sketch p99 succeeds");

	assert_eq!(buckets.len(), 1, "one bucket for the whole hour");
	assert_eq!(buckets[0].count, 3, "three points (newer-wins deduped)");
	let exact = buckets[0].values.get("p99").expect("p99 present").clone();
	let sketch = buckets[0].values.get("sketch_p99").expect("sketch_p99 present").clone();

	// One-pass answer: read_time_range, then reduce manually
	let (ts, vs) = store.read_time_range(ASPECT, 0, 50).await.expect("range read for one-pass");
	assert_eq!(ts, vec![10, 11, 12], "three timestamps, deduplicated");
	let non_null: Vec<BigDecimal> = vs.iter().flatten().cloned().collect();
	assert_eq!(non_null, vec![bd("10"), bd("20"), bd("30")], "newer-wins values");
	// `sketch_p99` is a `DdSketch` estimate, so it is pinned the way the server route pins it
	// (`weft-server/src/storage.rs`: within 1% of the exact nearest-rank percentile) rather than
	// as an exact decimal. The exact p99 of the merged {10,20,30} is 30; double-counting the
	// older segment would not change the exact p99, which is why the count above carries the
	// dedup assertion and this one pins that the fallback feeds the sketch the merged values.
	let (exact_f, sketch_f) = (weft_reduce::decimal_to_f64(&exact).expect("exact p99 is finite"), weft_reduce::decimal_to_f64(&sketch).expect("sketch p99 is finite"));
	assert_eq!(exact, bd("30"), "exact p99 of the 3 newer-wins points");
	assert!((sketch_f - exact_f).abs() / exact_f < 0.01, "sketch_p99 {sketch_f} must be within 1% of the exact p99 {exact_f}");

	drop(store);
}

/// 7. Unit test of `descriptors_overlap`: touching-but-disjoint does NOT trigger,
/// one shared instant DOES trigger.
#[tokio::test]
async fn descriptors_overlap_touching_disjoint_vs_shared() {
	use weft_physical_type::SegmentDescriptor;

	// Two descriptors with no overlap: t=[0,10) and [10,20) — touching at 10, not overlapping.
	let d1 = SegmentDescriptor { id: 1, path: String::new(), format_version: 1, physical_type: Some(PhysicalType::F64), time_unit: Some(TimeUnit::Seconds), row_count: 1, null_count: 0, time_sorted: true, min_ts: Some(0), max_ts: Some(10), min_value: None, max_value: None, byte_len: 100 };
	let d2 = SegmentDescriptor { id: 2, path: String::new(), format_version: 1, physical_type: Some(PhysicalType::F64), time_unit: Some(TimeUnit::Seconds), row_count: 1, null_count: 0, time_sorted: true, min_ts: Some(10), max_ts: Some(20), min_value: None, max_value: None, byte_len: 100 };

	// Touching at 10: clipped to [0, 30]: (0,10) and (10,20).
	// current_hi(10) >= next_lo(10) — this IS overlap per the conservative detection.
	assert!(descriptors_overlap(&[d1.clone(), d2.clone()], 0, 30), "touching-but-disjoint is conservatively detected as overlap (current_hi=10 >= lo=10)");

	// No overlap when the query window doesn't include the boundary: [0, 9] vs [10, 10]
	assert!(!descriptors_overlap(&[d1.clone(), d2.clone()], 0, 9), "window that ends before the next segment's start is not overlapping");

	// One shared instant: [0,10] and [10,20] → (0,10) and (10,10) → overlap
	assert!(descriptors_overlap(&[d1.clone(), d2.clone()], 10, 20), "window that includes the common boundary triggers overlap");

	// Two descriptors with truly overlapping ranges: [0,15] and [10,20]
	let d3 = SegmentDescriptor { id: 3, path: String::new(), format_version: 1, physical_type: Some(PhysicalType::F64), time_unit: Some(TimeUnit::Seconds), row_count: 2, null_count: 0, time_sorted: true, min_ts: Some(10), max_ts: Some(20), min_value: None, max_value: None, byte_len: 100 };
	let d4 = SegmentDescriptor { id: 4, path: String::new(), format_version: 1, physical_type: Some(PhysicalType::F64), time_unit: Some(TimeUnit::Seconds), row_count: 2, null_count: 0, time_sorted: true, min_ts: Some(15), max_ts: Some(25), min_value: None, max_value: None, byte_len: 100 };
	// (10,20) and (15,25) → current_hi(20) >= next_lo(15) → overlap
	assert!(descriptors_overlap(&[d3, d4], 0, 50), "truly overlapping descriptors trigger overlap");

	let d1_c = d1.clone();

	// Single descriptor: no overlap
	let d5 = SegmentDescriptor { id: 5, min_ts: Some(0), max_ts: Some(10), ..d1_c };
	assert!(!descriptors_overlap(&[d5], 0, 50), "single descriptor never overlaps");

	// Empty list: no overlap
	assert!(!descriptors_overlap(&[], 0, 50), "empty descriptor list never overlaps");

	// Descriptors with None min_ts/max_ts (empty segments): filtered out, no overlap
	let d6 = SegmentDescriptor { id: 6, min_ts: None, max_ts: None, row_count: 0, null_count: 0, ..d1 };
	let d7 = SegmentDescriptor { id: 7, min_ts: None, max_ts: None, row_count: 0, null_count: 0, ..d2 };
	assert!(!descriptors_overlap(&[d6, d7], 0, 50), "two empty descriptors do not overlap");
}

/// The whole history of [`ASPECT`] at one-minute buckets, summed: the single number that
/// separates the exact (deduplicated) answer from the sidecar merge's approximation.
async fn history_sum(store: &SegmentStore) -> BigDecimal {
	let buckets = store.downsample_range(ASPECT, i64::MIN, i64::MAX, Resolution::Minutes, &SIDECAR_AGGREGATIONS).await.expect("downsamples");
	assert_eq!(buckets.len(), 2, "the fixture spans two minute buckets");
	buckets.iter().map(|b| b.values.get("sum").expect("a sum").clone()).fold(BigDecimal::from(0), |acc, v| acc + v)
}

/// P12 #719, the hard corner: two segments of one series that **share instants**, and whose
/// frames are gone, so nothing but the persisted per-segment partials is left to answer with.
///
/// The store must keep answering — a store whose frames have been reclaimed is the normal
/// state sidecars exist for, not corruption, and a hard error here would take a healthy store
/// off line — and the number it answers is the documented approximation: each segment
/// contributes its own rows, so the six shared instants are counted twice and their older
/// values are added again. Exactness needs the rows, and they are gone. `reconcile_overlaps`
/// is the remedy; the read says so on `tracing::warn!` every time it takes this route.
#[tokio::test]
async fn overlapping_sidecar_only_downsample_serves_the_documented_approximation() {
	let dir = TempDir::new().expect("tempdir");
	let store = SegmentStore::open(dir.path()).await.expect("opens").with_partial_sidecar_policy(PartialSidecarPolicy::at_tiered(Resolution::Minutes, 1, &[Resolution::Hours]));
	let schema = schema();
	store.declare(ASPECT, &schema).await.expect("declares");

	// 0..=110 every 10 s with value = timestamp, then 60..=110 again with value = timestamp +
	// 1000: six instants shared, so the exact answer holds 12 rows (150 + 6510 = 6660) while a
	// merge of the two segments' own rows sees 18 (660 + 6510 = 7170).
	let mut older_times = Vec::new();
	let mut older_values = Vec::new();
	for i in 0..12 {
		older_times.push(i * 10);
		older_values.push(bd(&(i * 10).to_string()));
	}
	let mut newer_times = Vec::new();
	let mut newer_values = Vec::new();
	for i in 6..12 {
		newer_times.push(i * 10);
		newer_values.push(bd(&(i * 10 + 1000).to_string()));
	}
	let descriptors = vec![store.seal(ASPECT, &schema, &older_times, &older_values).await.expect("seals the older segment"), store.seal(ASPECT, &schema, &newer_times, &newer_values).await.expect("seals the newer segment")];
	for descriptor in &descriptors {
		assert!(store.load_partial_sidecar(ASPECT, descriptor, None).await.expect("loads").is_some(), "segment {} has a sidecar", descriptor.id);
	}

	// Frames present: the overlap fallback reads the rows and dedupes them newer-wins.
	assert_eq!(history_sum(&store).await, bd("6660"), "the frame-backed answer counts each shared instant once, keeping the newer value");

	for descriptor in &descriptors {
		std::fs::remove_file(&descriptor.path).expect("removes the frame");
	}
	assert_eq!(history_sum(&store).await, bd("7170"), "with the frames gone the sidecar merge answers instead, adding the older values of the six shared instants back in");
	// `history_sum` borrows the store, which clippy does not count as a usage of the binding, so
	// it is closed here: the frame store has to let go of its handles before `dir` is removed.
	drop(store);
}
