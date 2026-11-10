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
//! 2. Newer null over an older value — range uses newer-wins, point keeps older.
//! 3. Two different series split across overlapping segments (not merged together).
//! 4. Asymmetry pinned: range returns `None` where point returns the older value.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::SegmentStore;
use crate::{IngestBatch, TagSet};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

const ASPECT: &str = "overlap_test";

/// A lossless f64 schema in seconds.
fn schema() -> AspectSchema {
	AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)
}

/// A fresh store at `root` with [`ASPECT`] declared (lossless f64, seconds).
async fn store_at(root: &Path) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare(ASPECT, &schema()).await.expect("declares");
	store
}

/// Ingest one dense batch (one segment) of `rows` (value = timestamp as string).
async fn ingest(store: &SegmentStore, key: &str, ts: &[i64], vs: &[BigDecimal]) {
	store.ingest(ASPECT, &IngestBatch::dense(key, ts.to_vec(), vs.to_vec())).await.expect("ingests");
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

	// Range read: [10, 30] should return (10, 20, 30) with values (10, 200, 30)
	let (ts, vs) = store.read_time_range(ASPECT, 10, 30).await.expect("range read");
	assert_eq!(ts, vec![10, 20, 30], "timestamps: no duplicates");
	assert_eq!(vs, vec![Some(bd("10")), Some(bd("200")), Some(bd("30"))], "newer wins at 20");

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

	// Range read: *time* prune opens the all-null segment, newer-wins gives `None` at both.
	let (ts, vs) = store.read_time_range(ASPECT, 10, 20).await.expect("range read");
	assert_eq!(ts, vec![10, 20], "timestamps deduplicated");
	assert_eq!(vs, vec![None, None], "range: newer all-null segment's nulls mask older values");

	// Point read: *present* prune drops the all-null segment, older value at 10 survives.
	assert_eq!(store.read_point(ASPECT, 10).await.expect("point at 10"), Some(bd("10")), "point: all-null segment dropped, older value survives — deliberate asymmetry");

	drop(store);
}

/// ------------------------------------------------------------------
/// Case 2d: older segment's row survives when newer segment has NO row at that instant.
///
/// The newer segment has a row at timestamp 20 but no row at 10.  The older segment's
/// row at 10 must survive in both range and point reads.
#[tokio::test]
async fn older_row_survives_when_newer_has_no_row() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;

	// Older segment: timestamp 10 -> value 10
	ingest(&store, "a", &[10], &[bd("10")]).await;
	// Newer segment: timestamp 20 -> value 20 (no row at 10)
	ingest(&store, "b", &[20], &[bd("20")]).await;

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
