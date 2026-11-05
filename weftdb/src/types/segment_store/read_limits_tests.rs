//! ROB-13: the stored downsample and the batch point lookup are bounded in
//! the store itself, at 1.0 plan decision 12's numbers — 1,000,000 buckets
//! to a downsample, 10,000 instants to a batch lookup — so the bound holds
//! for every caller of the library, not only behind the HTTP handlers'
//! ceilings (ROB-12). The caps ride [`SegmentStoreOptions`], like TAG-7's
//! series cap, so a test (or an embedder) can hold a small cap and watch the
//! refusal happen on a small store.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use splimes::Resolution;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};
use weft_reduce::{Aggregation, BucketWidth};

use super::{SegmentStore, SegmentStoreOptions, DEFAULT_MAX_DOWNSAMPLE_BUCKETS, DEFAULT_MAX_POINT_BATCH};
use crate::{IngestBatch, StoreError};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

/// The aspect every test here writes.
const ASPECT: &str = "price";

/// A fresh store at `root` under `options`, with [`ASPECT`] declared
/// (lossless f64, seconds).
async fn store_at(root: &Path, options: SegmentStoreOptions) -> SegmentStore {
	let store = SegmentStore::open_with_options(root, options).await.expect("opens");
	store.declare(ASPECT, &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
	store
}

/// Ingest one dense batch (one segment) of `rows` under `key`.
async fn ingest(store: &SegmentStore, key: &str, rows: &[(i64, &str)]) {
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
	store.ingest(ASPECT, &IngestBatch::dense(key, ts, vs)).await.expect("ingests");
}

/// One row per second at `0..count`, value = its timestamp.
fn second_rows(count: i64) -> Vec<(i64, String)> {
	(0..count).map(|t| (t, t.to_string())).collect()
}

async fn ingest_seconds(store: &SegmentStore, key: &str, rows: &[(i64, String)]) {
	let (ts, vs): (Vec<i64>, Vec<BigDecimal>) = rows.iter().map(|(t, v)| (*t, bd(v))).unzip();
	store.ingest(ASPECT, &IngestBatch::dense(key, ts, vs)).await.expect("ingests");
}

#[test]
fn the_default_caps_are_decision_12s_numbers() {
	let options = SegmentStoreOptions::default();
	assert_eq!(options.max_downsample_buckets, 1_000_000, "decision 12: downsample buckets 1,000,000");
	assert_eq!(options.max_point_batch, 10_000, "decision 12: at-multi 10,000 instants");
	assert_eq!(DEFAULT_MAX_DOWNSAMPLE_BUCKETS, 1_000_000);
	assert_eq!(DEFAULT_MAX_POINT_BATCH, 10_000);
}

#[tokio::test]
async fn a_point_batch_at_the_cap_answers_and_one_over_is_refused() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_point_batch(3)).await;
	ingest(&store, "a", &[(0, "1"), (10, "2"), (20, "3"), (30, "4")]).await;

	// At the cap the batch answers, aligned to the query order.
	let values = store.read_points(ASPECT, &[30, 0, 25]).await.expect("at the cap");
	assert_eq!(values, vec![Some(bd("4")), Some(bd("1")), None]);

	// One over the cap the batch is refused as the typed error, naming both
	// numbers, before any segment is read.
	let err = store.read_points(ASPECT, &[0, 10, 20, 30]).await.expect_err("one over the cap");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::PointBatchTooLarge { aspect, instants: 4, limit: 3 }) if aspect == ASPECT), "{err:#}");

	// The single lookup names one instant and is untouched by the batch cap.
	assert_eq!(store.read_point(ASPECT, 20).await.expect("single lookup"), Some(bd("3")));
}

#[tokio::test]
async fn the_default_point_batch_cap_refuses_the_10001st_instant() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default()).await;
	ingest(&store, "a", &[(0, "1")]).await;

	let at_cap: Vec<i64> = (0..10_000).collect();
	assert_eq!(store.read_points(ASPECT, &at_cap).await.expect("at the default cap").len(), 10_000);
	let over_cap: Vec<i64> = (0..10_001).collect();
	let err = store.read_points(ASPECT, &over_cap).await.expect_err("one over the default cap");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::PointBatchTooLarge { instants: 10_001, limit: 10_000, .. })), "{err:#}");
}

#[tokio::test]
async fn a_downsample_at_the_cap_answers_and_one_bucket_over_is_refused() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_downsample_buckets(4)).await;
	ingest_seconds(&store, "a", &second_rows(5)).await;
	let aggs = [Aggregation::Avg];

	// Four buckets at one-second resolution fit the cap exactly.
	let buckets = store.downsample_range(ASPECT, 0, 3, Resolution::Seconds, &aggs).await.expect("at the cap");
	assert_eq!(buckets.len(), 4);

	// The fifth bucket crosses it: refused with the count that crossed and
	// the cap, from the single segment's own partial.
	let err = store.downsample_range(ASPECT, 0, 4, Resolution::Seconds, &aggs).await.expect_err("one bucket over the cap");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::DownsampleTooLarge { aspect, buckets: 5, limit: 4 }) if aspect == ASPECT), "{err:#}");

	// A refusal is not a poison: the store still answers under the cap.
	let buckets = store.downsample_range(ASPECT, 0, 3, Resolution::Seconds, &aggs).await.expect("still readable");
	assert_eq!(buckets.len(), 4);
}

#[tokio::test]
async fn the_merge_of_two_segments_is_bounded_as_it_folds() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_downsample_buckets(4)).await;
	// Two segments of three one-second buckets each: each segment's partial
	// fits the cap, their merge does not.
	ingest_seconds(&store, "a", &second_rows(3)).await;
	let shifted: Vec<(i64, String)> = (3..6).map(|t| (t, t.to_string())).collect();
	ingest_seconds(&store, "b", &shifted).await;
	let aggs = [Aggregation::Avg];

	let err = store.downsample_range(ASPECT, 0, 5, Resolution::Seconds, &aggs).await.expect_err("the merged grid is over the cap");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::DownsampleTooLarge { buckets: 6, limit: 4, .. })), "{err:#}");
}

#[tokio::test]
async fn a_width_downsample_is_bounded_on_the_grid_it_returns() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path(), SegmentStoreOptions::default().with_max_downsample_buckets(4)).await;
	ingest_seconds(&store, "a", &second_rows(10)).await;
	let aggs = [Aggregation::Avg];

	// Ten one-second buckets coarsened by 2 are a five-bucket grid: over the
	// cap of 4. The refusal comes from the fold itself, whose cap is the
	// output cap scaled by the coarsen count (4 x 2 unit buckets): a unit
	// fold over that is a returned grid over the cap, necessarily, since a
	// coarsened bucket holds at most `count` unit buckets.
	let width = BucketWidth::new(Resolution::Seconds, 2).expect("a width");
	let err = store.downsample_range_width(ASPECT, 0, 9, width, &aggs).await.expect_err("the coarsened grid is over the cap");
	assert!(matches!(err.downcast_ref::<StoreError>(), Some(StoreError::DownsampleTooLarge { buckets: 10, limit: 8, .. })), "{err:#}");

	// Coarsened by 3 the same rows are a four-bucket grid and answer — the
	// bound is the returned grid, not the unit fold behind it (which held
	// ten unit buckets, over the cap, on its way to four).
	let width = BucketWidth::new(Resolution::Seconds, 3).expect("a width");
	let buckets = store.downsample_range_width(ASPECT, 0, 9, width, &aggs).await.expect("the coarsened grid fits");
	assert_eq!(buckets.len(), 4);
}
