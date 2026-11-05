//! TAG-8, the read lane of tagged series: every store read targets exactly one series
//! (tags D4). The existing reads mean series 0, so an untagged caller of a tagged aspect sees
//! exactly what it saw before the tagged rows arrived; the `_of` reads take a [`SeriesId`],
//! and [`resolve_series`](SegmentStore::resolve_series) turns a [`SeriesSelector`] into one.

use std::{path::Path, str::FromStr};

use bigdecimal::BigDecimal;
use splimes::{Interpolator, Resolution, Spline};
use tempfile::TempDir;
use weft_physical_type::{timestamp::TimeUnit, AspectSchema, PhysicalType};
use weft_reduce::Aggregation;

use super::SegmentStore;
use crate::{IngestBatch, SeriesId, SeriesKey, SeriesSelector, StoreError, TagSet};

fn bd(s: &str) -> BigDecimal {
	BigDecimal::from_str(s).expect("parses")
}

const ASPECT: &str = "price";

fn tags(pairs: &[(&str, &str)]) -> TagSet {
	TagSet::from_pairs(pairs.iter().copied()).expect("valid tags")
}

/// A fresh store at `root` with [`ASPECT`] declared (lossless f64, seconds).
async fn store_at(root: &Path) -> SegmentStore {
	let store = SegmentStore::open(root).await.expect("opens");
	store.declare(ASPECT, &AspectSchema::new(PhysicalType::F64, bd("0"), TimeUnit::Seconds)).await.expect("declares");
	store
}

/// Ingest `rows` (value = `scale * t`) at `0, 10, .., 90` into `groups` under `key`.
async fn ingest(store: &SegmentStore, key: &str, groups: TagSet, scale: i64) {
	let ts: Vec<i64> = (0..10).map(|i| i * 10).collect();
	let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(t * scale)).collect();
	store.ingest(ASPECT, &IngestBatch::dense(key, ts, vs).with_groups(groups)).await.expect("ingests");
}

async fn id_of(store: &SegmentStore, groups: &TagSet) -> SeriesId {
	store.index.series_id_for(ASPECT, groups.series_key()).await.expect("reads the catalog").expect("registered")
}

/// The untagged reads of an aspect that also holds tagged series answer series 0 alone, and
/// byte-for-byte what an untagged-only store answers.
#[tokio::test]
async fn untagged_reads_see_series_zero_only() {
	let (mixed_dir, plain_dir) = (TempDir::new().expect("tempdir"), TempDir::new().expect("tempdir"));
	let (mixed, plain) = (store_at(mixed_dir.path()).await, store_at(plain_dir.path()).await);
	ingest(&mixed, "a", tags(&[("host", "a")]), 100).await;
	ingest(&mixed, "u", TagSet::new(), 1).await;
	ingest(&mixed, "b", tags(&[("host", "b")]), 1000).await;
	ingest(&plain, "u", TagSet::new(), 1).await;

	assert_eq!(mixed.read_time_range(ASPECT, 0, 100).await.expect("reads"), plain.read_time_range(ASPECT, 0, 100).await.expect("reads"));
	assert_eq!(mixed.read_point(ASPECT, 50).await.expect("reads"), Some(bd("50")));
	assert_eq!(mixed.read_points(ASPECT, &[10, 20, 25]).await.expect("reads"), plain.read_points(ASPECT, &[10, 20, 25]).await.expect("reads"));
	assert_eq!(mixed.read_value_range(ASPECT, &bd("0"), &bd("100000")).await.expect("reads"), plain.read_value_range(ASPECT, &bd("0"), &bd("100000")).await.expect("reads"));
	let aggs = [Aggregation::Avg, Aggregation::Max];
	assert_eq!(mixed.downsample_range(ASPECT, 0, 100, Resolution::Minutes, &aggs).await.expect("downsamples"), plain.downsample_range(ASPECT, 0, 100, Resolution::Minutes, &aggs).await.expect("downsamples"));
	let width = weft_reduce::BucketWidth::new(Resolution::Seconds, 30).expect("a width");
	assert_eq!(mixed.downsample_range_width(ASPECT, 0, 100, width, &aggs).await.expect("downsamples"), plain.downsample_range_width(ASPECT, 0, 100, width, &aggs).await.expect("downsamples"));
	let linear = Interpolator::new(Spline::Linear, Resolution::Seconds);
	let (got, want) = (mixed.interpolate_range(ASPECT, 0, 90, 0, linear).await.expect("interpolates"), plain.interpolate_range(ASPECT, 0, 90, 0, linear).await.expect("interpolates"));
	assert_eq!(got.values(), want.values());
	let (got, want) = (mixed.interpolate_range_f64(ASPECT, 0, 90, 0, linear).await.expect("interpolates"), plain.interpolate_range_f64(ASPECT, 0, 90, 0, linear).await.expect("interpolates"));
	assert_eq!(got.values(), want.values());
}

/// Each `_of` read answers its own series' rows and nothing else.
#[tokio::test]
async fn of_reads_answer_one_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	ingest(&store, "u", TagSet::new(), 1).await;
	ingest(&store, "a", tags(&[("host", "a")]), 100).await;
	ingest(&store, "b", tags(&[("host", "b")]), 1000).await;
	let (a, b) = (id_of(&store, &tags(&[("host", "a")])).await, id_of(&store, &tags(&[("host", "b")])).await);
	assert_ne!(a, b);

	let (ts, vs) = store.read_time_range_of(ASPECT, a, 0, 100).await.expect("reads");
	assert_eq!(ts, (0..10).map(|i| i * 10).collect::<Vec<_>>());
	assert_eq!(vs, ts.iter().map(|t| Some(BigDecimal::from(t * 100))).collect::<Vec<_>>());
	assert_eq!(store.read_point_of(ASPECT, b, 50).await.expect("reads"), Some(bd("50000")));
	assert_eq!(store.read_points_of(ASPECT, a, &[10, 20, 25]).await.expect("reads"), vec![Some(bd("1000")), Some(bd("2000")), None]);
	let (ts, vs) = store.read_value_range_of(ASPECT, b, &bd("0"), &bd("20000")).await.expect("reads");
	assert_eq!((ts, vs), (vec![0, 10, 20], vec![bd("0"), bd("10000"), bd("20000")]), "the resident index is the series' own");
	let buckets = store.downsample_range_of(ASPECT, a, 0, 100, Resolution::Minutes, &[Aggregation::Max]).await.expect("downsamples");
	assert_eq!(buckets.iter().map(|b| b.values["max"].clone()).collect::<Vec<_>>(), vec![bd("5000"), bd("9000")]);
	let width = weft_reduce::BucketWidth::new(Resolution::Seconds, 100).expect("a width");
	let buckets = store.downsample_range_width_of(ASPECT, b, 0, 100, width, &[Aggregation::Max]).await.expect("downsamples");
	assert_eq!(buckets.iter().map(|b| b.values["max"].clone()).collect::<Vec<_>>(), vec![bd("90000")]);
	let linear = Interpolator::new(Spline::Linear, Resolution::Seconds);
	let curve = store.interpolate_range_of(ASPECT, a, 40, 50, 0, linear).await.expect("interpolates");
	assert_eq!(curve.values()[5], bd("4500"));
	let curve = store.interpolate_range_f64_of(ASPECT, b, 40, 50, 0, linear).await.expect("interpolates");
	assert!((curve.values()[5] - 45_000.0).abs() < 1e-9);

	// An id the aspect never registered reads empty, as a window with no rows does.
	assert_eq!(store.read_time_range_of(ASPECT, SeriesId(99), 0, 100).await.expect("reads"), (vec![], vec![]));
	assert_eq!(store.read_point_of(ASPECT, SeriesId(99), 50).await.expect("reads"), None);
}

/// `resolve_series`: `untagged()` is series 0 without the catalog; an exact selector finds its
/// series; nothing matched is `None`; two or more is `AmbiguousSeries`; series 0 is a
/// candidate only when the selector matches the empty set and series 0 holds rows.
#[tokio::test]
async fn selectors_resolve_to_one_series() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	let sel = |pairs: &[(&str, Option<&str>)]| SeriesSelector::new(pairs.iter().copied()).expect("valid selector");
	let exact = |pairs: &[(&str, Option<&str>)]| SeriesSelector::exact(pairs.iter().copied()).expect("valid selector");

	assert_eq!(store.resolve_series(ASPECT, &SeriesSelector::untagged()).await.expect("resolves"), Some(SeriesId::UNTAGGED), "untagged() needs no catalog");
	assert_eq!(store.resolve_series(ASPECT, &SeriesSelector::all()).await.expect("resolves"), None, "an empty aspect has no series");

	ingest(&store, "a", tags(&[("host", "a"), ("dc", "x")]), 100).await;
	let a = id_of(&store, &tags(&[("host", "a"), ("dc", "x")])).await;
	// Series 0 holds no rows, so a loose selector that matches the empty set finds `a` alone.
	assert_eq!(store.resolve_series(ASPECT, &SeriesSelector::all()).await.expect("resolves"), Some(a));
	assert_eq!(store.resolve_series(ASPECT, &sel(&[("host", Some("a"))])).await.expect("resolves"), Some(a));
	assert_eq!(store.resolve_series(ASPECT, &exact(&[("host", Some("a"))])).await.expect("resolves"), None, "exact needs every tag");
	assert_eq!(store.resolve_series(ASPECT, &exact(&[("dc", Some("x")), ("host", Some("a"))])).await.expect("resolves"), Some(a));

	ingest(&store, "u", TagSet::new(), 1).await;
	let err = store.resolve_series(ASPECT, &SeriesSelector::all()).await.expect_err("series 0 and `a`");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::AmbiguousSeries { aspect: ASPECT.to_string(), matched: 2 }), "{err:#}");
	assert_eq!(store.resolve_series(ASPECT, &sel(&[("host", None)])).await.expect("resolves"), Some(SeriesId::UNTAGGED), "only series 0 lacks `host`");

	ingest(&store, "b", tags(&[("host", "b"), ("dc", "x")]), 1000).await;
	let err = store.resolve_series(ASPECT, &sel(&[("dc", Some("x"))])).await.expect_err("`a` and `b`");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::AmbiguousSeries { aspect: ASPECT.to_string(), matched: 2 }), "{err:#}");
	assert_eq!(store.resolve_series(ASPECT, &sel(&[("host", Some("c"))])).await.expect("resolves"), None);
	assert_eq!(store.resolve_series("../x", &SeriesSelector::all()).await.map(|_| ()).expect_err("an invalid name").downcast_ref::<StoreError>(), None);

	// The resolved id feeds the `_of` reads.
	let b = store.resolve_series(ASPECT, &sel(&[("host", Some("b"))])).await.expect("resolves").expect("one series");
	assert_eq!(store.read_point_of(ASPECT, b, 90).await.expect("reads"), Some(bd("90000")));
}

/// The library gate is open: a tagged batch commits through the public `ingest`, and a
/// reserved `__` key, which only a `TagSet` converted from a stored key can carry, is
/// refused before anything is written.
#[tokio::test]
async fn the_library_gate_admits_tags_but_not_reserved_keys() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	ingest(&store, "a", tags(&[("host", "a")]), 1).await;
	let reserved = TagSet::from(SeriesKey::from_canonical(b"__dim\x1fs\x1ehost\x1fa").expect("a stored reserved key"));
	let err = store.ingest(ASPECT, &IngestBatch::dense("r", vec![0], vec![bd("1")]).with_groups(reserved)).await.expect_err("a reserved key");
	assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::ReservedTagKey { aspect: ASPECT.to_string(), key: "__dim".to_string() }), "{err:#}");
	assert_eq!(store.segment_count(ASPECT).await.expect("counts"), 1, "the refusal wrote nothing");
	assert_eq!(store.index.series_count(ASPECT).await.expect("counts"), 1, "and registered nothing");
}

/// `series_tags` (TAG-10): series 0 is the empty set without the catalog, a registered id is
/// its decoded key, and an id never handed out is `None`.
#[tokio::test]
async fn series_tags_reads_back_the_catalog_key() {
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	assert_eq!(store.series_tags(ASPECT, SeriesId::UNTAGGED).await.expect("reads"), Some(TagSet::new()));
	let groups = tags(&[("host", "a"), ("dc", "x")]);
	ingest(&store, "a", groups.clone(), 1).await;
	let a = id_of(&store, &groups).await;
	assert_eq!(store.series_tags(ASPECT, a).await.expect("reads"), Some(groups));
	assert_eq!(store.series_tags(ASPECT, SeriesId(a.0 + 1)).await.expect("reads"), None);
	let invalid = store.series_tags("../x", a).await;
	drop(store);
	assert!(invalid.is_err(), "an invalid aspect name");
}

/// Phase 4 tag pruning: a tagged read of an aspect with 1,000 series, every one over the
/// same window, opens only its own series' frames. Every other series' frame is
/// overwritten with garbage first, so a read that opened one would fail its checksum.
/// The selector resolves through the catalog cache, which a registration keeps current.
#[tokio::test]
async fn a_tagged_read_among_a_thousand_series_opens_only_its_own_frames() {
	const SERIES: usize = 1_000;
	let dir = TempDir::new().expect("tempdir");
	let store = store_at(dir.path()).await;
	for i in 0..SERIES {
		let host = format!("h{i:04}");
		let ts: Vec<i64> = (0..4).map(|k| k * 10).collect();
		let vs: Vec<BigDecimal> = ts.iter().map(|t| BigDecimal::from(t + i64::try_from(i).expect("small"))).collect();
		store.ingest(ASPECT, &IngestBatch::dense(format!("k{i}"), ts, vs).with_groups(tags(&[("host", &host)]))).await.expect("ingests");
	}
	let wanted = store.resolve_series(ASPECT, &SeriesSelector::exact([("host", Some("h0421"))]).expect("valid")).await.expect("resolves").expect("one series");
	let rows = store.index.prune_rows_by_time(ASPECT, i64::MIN, i64::MAX).await.expect("prunes");
	assert_eq!(rows.iter().map(|row| row.series_id).collect::<std::collections::BTreeSet<_>>().len(), SERIES, "one series per ingest");
	let mut corrupted = 0;
	for row in rows.iter().filter(|row| row.series_id != wanted.0) {
		std::fs::write(store.frame_path(&row.desc).expect("a frame path"), b"not a frame").expect("overwrites");
		corrupted += 1;
	}
	assert!(corrupted >= SERIES - 1);
	let (ts, vs) = store.read_time_range_of(ASPECT, wanted, 0, 30).await.expect("reads only its own frames");
	assert_eq!(ts, vec![0, 10, 20, 30]);
	assert_eq!(vs, [421, 431, 441, 451].iter().map(|v| Some(BigDecimal::from(*v))).collect::<Vec<_>>());
	assert_eq!(store.read_point_of(ASPECT, wanted, 20).await.expect("reads"), Some(bd("441")));
	// A series registered after the catalog was cached resolves too.
	store.ingest(ASPECT, &IngestBatch::dense("late", vec![0], vec![bd("7")]).with_groups(tags(&[("host", "late")]))).await.expect("ingests");
	let late = store.resolve_series(ASPECT, &SeriesSelector::exact([("host", Some("late"))]).expect("valid")).await.expect("resolves").expect("the new series");
	assert_eq!(store.read_point_of(ASPECT, late, 0).await.expect("reads"), Some(bd("7")));
}
