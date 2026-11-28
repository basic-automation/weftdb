//! TAG-12: [`SegmentStore::read_latest_of`], the per-series latest-row read behind TSBS's
//! `lastpoint`. Every case checks it against the last element of a whole-series
//! [`read_time_range_of`](SegmentStore::read_time_range_of), the read it must equal.
//!
//! Cases:
//! 1. No rows, and an id the aspect never registered, read `None`.
//! 2. Many sequential segments: the last row of the last segment.
//! 3. A later seal rewrites the latest instant: newer-wins, as the range read.
//! 4. A newer null at the latest instant is the answer, as the range read has it.
//! 5. An older segment reaching past a later seal still supplies the latest row (the mutant
//!    "read only the newest segment" fails here).
//! 6. Two tagged series answer independently, and the untagged form reads series 0.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};

use super::SegmentStore;
use crate::{IngestBatch, SeriesId, TagSet, UpsertKeys};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

const ASPECT: &str = "latest_test";

/// A fresh store at `root` with [`ASPECT`] declared (lossless f64, seconds), keyed by `host`
/// so an overlapping write is an update (as `overlap_read_tests.rs` declares it).
async fn store_at(root: &Path) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	let keys = UpsertKeys::new(["host"]).expect("valid keys");
	store.declare_with_upsert_keys(ASPECT, &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds), Some(&keys)).await.expect("declares");
	store
}

async fn ingest(store: &SegmentStore, key: &str, groups: &TagSet, ts: &[i64], vs: &[Option<BigDecimal>]) {
	store.ingest(ASPECT, &IngestBatch::new(key, ts.to_vec(), vs.to_vec()).with_groups(groups.clone())).await.expect("ingests");
}

async fn id_of(store: &SegmentStore, groups: &TagSet) -> SeriesId {
	store.index.series_id_for(ASPECT, groups.series_key()).await.expect("reads the catalog").expect("registered")
}

/// The answer `read_latest_of` must equal: the last row of the whole-series range read.
async fn whole_series_last(store: &SegmentStore, series: SeriesId) -> Option<(i64, Option<BigDecimal>)> {
	let (ts, vs) = store.read_time_range_of(ASPECT, series, i64::MIN, i64::MAX).await.expect("range read");
	ts.last().copied().zip(vs.into_iter().next_back())
}

async fn latest(store: &SegmentStore, series: SeriesId) -> Option<(i64, Option<BigDecimal>)> {
	let got = store.read_latest_of(ASPECT, series).await.expect("latest read");
	assert_eq!(got, whole_series_last(store, series).await, "the latest read equals the whole-series range read's last row");
	got
}

#[tokio::test]
async fn an_empty_or_unknown_series_has_no_latest_row() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	assert_eq!(latest(&store, SeriesId::UNTAGGED).await, None);
	assert_eq!(store.read_latest(ASPECT).await.expect("latest read"), None);
	ingest(&store, "a", &TagSet::new(), &[10], &[Some(bd("1"))]).await;
	assert_eq!(latest(&store, SeriesId(987_654)).await, None, "an id the aspect never registered reads empty");
}

#[tokio::test]
async fn many_sequential_segments_answer_the_last_row() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	for batch in 0..8_i64 {
		let ts: Vec<i64> = (0..50).map(|i| batch * 1_000 + i * 10).collect();
		let vs: Vec<Option<BigDecimal>> = ts.iter().map(|t| Some(BigDecimal::from(t * 3))).collect();
		ingest(&store, &format!("b{batch}"), &TagSet::new(), &ts, &vs).await;
	}
	assert_eq!(store.index.series_rows(ASPECT, SeriesId::UNTAGGED).await.expect("index rows").len(), 8, "eight sealed segments");
	assert_eq!(latest(&store, SeriesId::UNTAGGED).await, Some((7_490, Some(BigDecimal::from(22_470)))));
}

#[tokio::test]
async fn a_later_seal_at_the_latest_instant_wins() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	ingest(&store, "old", &TagSet::new(), &[10, 20, 30], &[Some(bd("10")), Some(bd("20")), Some(bd("30"))]).await;
	ingest(&store, "new", &TagSet::new(), &[30], &[Some(bd("300"))]).await;
	assert_eq!(latest(&store, SeriesId::UNTAGGED).await, Some((30, Some(bd("300")))));
}

#[tokio::test]
async fn a_newer_null_at_the_latest_instant_is_the_answer() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	ingest(&store, "old", &TagSet::new(), &[10, 20], &[Some(bd("10")), Some(bd("20"))]).await;
	ingest(&store, "new", &TagSet::new(), &[20], &[None]).await;
	assert_eq!(latest(&store, SeriesId::UNTAGGED).await, Some((20, None)), "the range read's newer-wins null, not the older value");
}

#[tokio::test]
async fn an_older_segment_reaching_past_a_later_seal_supplies_the_row() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	// Sealed first and spanning [0, 100]; the later seal covers only [40, 50].
	ingest(&store, "wide", &TagSet::new(), &[0, 50, 100], &[Some(bd("0")), Some(bd("5")), Some(bd("100"))]).await;
	ingest(&store, "narrow", &TagSet::new(), &[40, 50], &[Some(bd("4")), Some(bd("500"))]).await;
	assert_eq!(store.index.series_latest_ts(ASPECT, SeriesId::UNTAGGED).await.expect("index"), Some(100));
	assert_eq!(latest(&store, SeriesId::UNTAGGED).await, Some((100, Some(bd("100")))));
}

#[tokio::test]
async fn tagged_series_answer_independently() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	let (a, b) = (TagSet::from_pairs([("host", "a")]).expect("tags"), TagSet::from_pairs([("host", "b")]).expect("tags"));
	ingest(&store, "a", &a, &[10, 20], &[Some(bd("1")), Some(bd("2"))]).await;
	ingest(&store, "b", &b, &[10, 20, 30], &[Some(bd("10")), Some(bd("20")), Some(bd("30"))]).await;
	ingest(&store, "u", &TagSet::new(), &[5], &[Some(bd("-1"))]).await;
	assert_eq!(latest(&store, id_of(&store, &a).await).await, Some((20, Some(bd("2")))));
	assert_eq!(latest(&store, id_of(&store, &b).await).await, Some((30, Some(bd("30")))));
	assert_eq!(store.read_latest(ASPECT).await.expect("latest read"), Some((5, Some(bd("-1")))), "the untagged form reads series 0");
}
